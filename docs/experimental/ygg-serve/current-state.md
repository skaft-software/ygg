# Ygg Serve current state and handoff

This document is the durable fresh-context handoff for the experimental
graphical `ygg serve` effort.

## Bottom line

`ygg serve` is a substantial, testable experimental vertical slice backed by
real Ygg sessions. It is not yet a finished coding workbench, and the
LAN/native-client layer is still specification-only.

The immediate release blockers are:

- the forward package-boundary gate passes, but the branch's older stacked
  history still needs a target-branch review rather than being hidden in a
  blanket allowlist;
- final acceptance against a user's actual configured model provider remains
  outstanding;
- the experimental release workflow is defined but has not yet been executed
  against a release tag, so no signed serve artifact is published; and
- LAN/native delivery and several workbench-level product surfaces remain
  deliberately out of scope or incomplete.

## Current status at a glance

| Area | Reality today |
| --- | --- |
| Real local Ygg agent sessions | Yes |
| Streaming, tools, approvals, stop, steer, follow-up | Yes |
| Multiple independent sessions | Yes |
| Exception-driven command center | Aggregate needs-you, working, review, complete, and evidence-backed pull-request state; prioritized task queue; task/project search; focused-task handoff |
| Reconnect, replay, resume, and branch checkout | Yes |
| Attachments and prompt documents | PNG/JPEG/GIF/WebP images plus bounded text, Markdown, and PDF context; no audio |
| Sources, diffs, and outputs | Real but limited to specific built-in tools |
| Live generated-site previews | Fixture UI exists; production capability is off |
| Projects and folder management | Durable private multi-project registry, trust/default/archive/session binding, repository context, and trusted file browsing; a host-native folder picker is not yet available |
| Session retention | Archive, recoverable trash/restore, guarded permanent deletion, and startup recovery |
| Context and compaction telemetry | Authoritative replayable accounting in the agent, protocol, runtime inspector, and composer |
| Integrated terminal | Production PTY panel when session process execution is allowed |
| Extension, skills, MCP, or LSP GUI | Composer discovery supports trusted skills, prompt templates, and enabled extension commands; no dedicated management GUI, MCP, or LSP |
| Child-agent visualization | Not implemented; the runtime does not expose it |
| LAN-connected devices | Designed, not implemented |
| macOS, iOS, and Android applications | Designed; no app projects or signed builds |
| Clean-room coding-workbench layout | Implemented across desktop, tablet, and phone |
| Ready to merge | No |
| Ready for experimental user testing | Yes |

## Settled product

The settled product is not "Ygg Workbench" as a separate mode. It is simply
graphical `ygg`:

- `ygg serve` runs Ygg headlessly.
- There is one repository-oriented task lifecycle with two complementary views:
  an exception-driven command center for supervision and a focused transcript
  for execution.
- Opening the root creates a fresh provisional task. `/overview` opens the
  command center from session inventory without creating or opening a task, and
  an explicit session route restores a focused task.
- Previous, pinned, and running tasks appear in the sidebar.
- Different tasks are independent agent sessions and may run concurrently.
- There is no Chat, Code, Work, or Cowork mode selector.
- There is no synchronized TUI, terminal mirror, or terminal-window mode.
- A phone is eventually a companion/controller for the host, not a mobile
  agent runtime.
- The command center derives aggregate state, ordering, search text, and row
  previews from the existing session catalog. It does not create a second
  orchestration protocol or synthetic agent narrative.
- The transcript is primary within focused work. Sources, actions, outputs,
  previews, and progress appear only when structured events justify them.
- There is no account, login, hosted control plane, or outbound product
  telemetry. Local context, lifecycle, and usage accounting remain part of the
  workbench state.
- Ygg retains broad local authority by default. Network authentication remains
  separate from agent authority.
- The interface is a deterministic projection of real Ygg events. It does not
  add UI instructions to the model or ask another model to invent summaries or
  cards.

See the concise [product contract](README.md).

## How the effort evolved

The first frontend used a generic dashboard/switchboard layout with an evidence
shelf and runtime-oriented information architecture. It was rejected.

That exploration is preserved at:

- branch `archive/ygg-workbench-rejected-20260726`;
- commit `7ca26e3`; and
- approximately 20,751 added lines across 90 files.

Useful server and protocol ideas survived, but the frontend layout, user flow,
and visual language were restarted.

The replacement initially borrowed generic chat-product composition from
ChatGPT and Claude. That direction was useful for interaction behavior, but its
centered narrow composer, model-colored chrome, animated splash, soft cards,
and mixed visual skins did not read as a coding workbench.

The current direction is a clean-room coding workbench using familiar native
macOS and editor conventions: project and session hierarchy at left, a broad
transcript in the center, evidence at right, and an integrated pane-width
composer. No proprietary vendor code, bundle, or asset was copied into Ygg; the
interface remains original React and CSS built on Ygg's existing protocol and
renderers.

The command center does not restore the rejected switchboard. It is a compact
projection of the same task catalog used by the sidebar: aggregate exception
counts, a stable priority queue, and search. It introduces no evidence shelf,
runtime-centric mode, generated cards, or alternate execution path.

## Settled visual and interaction language

The UI iterations converged on these rules:

- Always spell the product `ygg`; keep the wordmark unboxed and unaccompanied
  by a decorative logo in the sidebar header.
- Use one opaque, neutral-dark workbench appearance. Do not use glass,
  translucency, or model-driven application colors. Rainbow color is reserved
  for the reasoning-effort control.
- The focused desktop shell is a real three-pane workbench: a 296px project/task
  sidebar, a broad center pane, and an optional 400px evidence pane. The command
  center uses the same sidebar with one broad supervisory surface. Distinct
  shaded surfaces separate panes without one-pixel divider lines.
- Label navigation as Tasks, retain project grouping, and keep each sidebar row
  to its title plus an optional PR mark. The PR mark appears only for structured
  `in_progress`, `ready`, or `merged` evidence; repository state, model metadata,
  and generic run status are not synthesized into the row.
- Keep the command center exception-driven: semantic status totals lead to one
  compact, aligned queue rather than a kanban board or a wall of agent cards.
  Failed, disconnected, attention-required, and review-ready work sorts ahead of
  healthy running or completed work.
- Status color is semantic: green means working or successful, amber means
  attention, and red means failure. Provider and model colors do not determine
  shell state.
- Keep the fresh task quiet: “New workspace task,” “What should we work on?”,
  and a short instruction replace the animated TUI/model splash.
- Compose the transcript like an engineering record, not a chat product. User
  turns and expanded tool calls use restrained tonal surfaces without thin card
  outlines; assistant prose and grouped tool evidence remain primary. Prior
  actions collapse into one concise activity summary while the latest streaming
  item stays visible on its own line. Reasoning, commands, metadata, output, and
  completion evidence remain available through disclosures.
- The composer spans the usable center pane, has restrained geometry, and stays
  visually attached to the work surface. It has no border, shimmer, perimeter
  chase, glass, or model-colored chrome, while focus remains visibly ringed.
- Use Local Grotesk and Local Mono from bundled Local Type System 0.53 by
  default. Their compact variable web fonts cover the full 400–700 interface
  range without separate weight files. Open counters, differentiated ambiguity
  forms, restrained lower-half gravity, and a shared authored construction
  grammar balance legibility with a playful DIY character without making a
  clinical accessibility claim. The visible type scale has two sizes: 14px
  interface text and 12px metadata. Monospace remains limited to code, paths,
  diffs, commands, and technical metadata. Popular device-installed font
  pairings and size preferences remain available.
- The model picker keeps the simple model/effort abstraction, with precise
  controls under Advanced. Ordinary effort fills the track in blue through and
  slightly behind the white thumb without a rounded-cap gap. Exact `xhigh` adds
  varied white particles that float locally within the blue fill. Exact `max`
  combines those particles with the animated rainbow and is the only rainbow
  state. Reduced motion freezes that rainbow and removes all particles. Changing
  models never changes the shell appearance.
- Do not duplicate Working or Activity indicators inside focused-task chrome.
  The optional right pane is limited to review, command history, progress,
  artifacts, and context backed by structured events.
- A dominant inspector opens source, output, image, and diff content without
  changing transport or transcript behavior.
- Mobile displays one primary surface at a time. Navigation, Activity, and the
  inspector become full-height overlays while the composer keeps all controls
  keyboard accessible.
- Connected Devices appears only when the host advertises that capability; do
  not invent account, machine, connection, or evidence state.

The typography policy and browser baselines enforce this workbench contract,
including full-shell desktop, performance transcript, mobile completion review,
and mobile inspector states.

## Architecture

The implemented shape is:

```text
React 19 web client
  -> versioned ygg-serve protocol and deterministic reducer
    -> loopback Rust host/service
      -> session supervisor
        -> one serialized actor per graphical session
          -> feature-gated ygg-coding-agent adapter
            -> one private App/Agent/session owner
```

See the detailed [architecture](architecture.md).

Intended package ownership is:

- `apps/web/` owns the shared React interface.
- `extensions/ygg-serve/` owns the protocol, transport, orchestration,
  security, durable evidence, and embedded assets.
- `crates/ygg-coding-agent/src/extensions/serve.rs` owns the smallest
  feature-gated adapter into Ygg's private `App`.
- Future packages under `apps/` own thin native shells.

The optional backend is deliberately excluded from the ordinary Cargo
workspace. The `serve` feature is disabled by default, so the normal TUI and
agent do not depend on the web surface. The default binary can install and
launch the separately packaged feature-enabled runtime.

The actual package and launch CLI supports:

```text
ygg extension install ygg-serve
ygg extension install --path <archive>
ygg extension list
ygg extension update ygg-serve
ygg extension remove ygg-serve
ygg serve
  --no-open
  --port <u16>
  --web-root <directory>
```

There is no implemented `--lan`, `--demo`, or `--local-only` switch.

From the repository root, the shortest direct development launch command is:

```console
cargo run --features serve -- serve
```

The feature remains necessary only when building the packaged runtime from
source. An ordinary release installation dispatches `ygg serve` to that runtime.
`--port 0` may be added to request an ephemeral port.

Configuration loading reports unknown global and trusted-project TOML keys with
source path, line, column, dotted key, and a bounded typo suggestion. Unknown
keys warn by default for compatibility. The global `--strict-config` flag,
`strict_config = true`, or `YGG_STRICT_CONFIG=true` makes the collected
unknown-key diagnostics fatal. Known compatibility aliases remain accepted.
See [Configuration diagnostics](../../design/config-diagnostics.md).

## What is genuinely implemented

### Real host and sessions

- A feature-gated `ygg serve` binary path.
- A loopback-only Axum host.
- An embedded frontend with asset digests.
- A one-use launch capability exchanged for an HttpOnly, SameSite=Strict
  cookie.
- Strict Host, Origin, and Fetch Metadata validation.
- No CORS or remote assets.
- Bounded HTTP, WebSocket, replay, resource, and attachment payloads.
- Fresh-session root behavior.
- Explicit session restoration and a durable, inventory-only `/overview`
  command-center route.
- Exception-prioritized active-task aggregation and task/project search using
  existing session summaries.
- Concurrent independent graphical sessions.
- Exactly one mutable `App` owner per session.
- A durable, owner-private project registry with opaque IDs, root-identity
  revalidation, explicit trust, defaults, archive, and session bindings.
- Session rename and pin plus active/archive/trash lifecycle views.
- Restore from trash and exact-phrase permanent deletion through a durable,
  crash-recoverable cleanup journal.
- Host-authoritative session titles and catalog updates.
- Authenticated bounded transcript search and redacted JSON export.
- Branch graph projection and safe idle-boundary branch checkout.

### Lifecycle and persistence safety

- Git probes and PTY shells own Unix process groups and use bounded graceful/
  forced descendant cleanup; retained output descriptors cannot hang shutdown.
- PTY output uses incremental UTF-8 decoding and valid-boundary replay
  truncation.
- Trusted project reads and writes use root-identity revalidation,
  descriptor-relative no-follow traversal, conflict checks, atomic replacement,
  content synchronization, and owning-directory synchronization.
- WebSocket connections and store initialization are generation-scoped, so
  stale callbacks, replay responses, and timers cannot replace newer state.
- Permanent deletion journals intent before the transcript boundary, rolls back
  interrupted pre-commit work, and retries committed cleanup idempotently after
  restart. It removes session-owned attachments, documents, resources, run
  records, goals, project bindings, and search data while retaining shared
  payloads and conversation-content-free inference accounting.
- Missing required stores fail permanent deletion before commit rather than
  producing a partially deleted session.

See [Serve lifecycle and safety](../../design/serve-lifecycle-safety.md) for the
full trust and recovery contracts.

### Agent interaction

- Real prompt submission through the coding-agent path.
- Streaming assistant text and reasoning.
- Structured tool calls, results, and progress.
- Approval and typed-input requests.
- Stop.
- Steering.
- Queued follow-up.
- Prior-turn edit, response retry (including model override), conversation
  fork, and whole-session fork at idle durable boundaries.
- Durable run outcomes.
- Authoritative context categories, response/tool lifecycle counters, and
  compaction start/finish/failure projections.
- Model catalog and model selection.
- Reasoning-effort selection.
- Composer `@` completion backed by trusted project-file IDs; selected files remain
  explicit context instead of being silently injected into user text.
- Session-scoped `/` discovery and typed idle-boundary invocation for built-in
  commands, prompt templates, host-admitted skills, and enabled extension
  commands.

Context and run lifecycle state is derived from the active agent run and
published as replayable full-state `context.updated` replacements. Polling does
not mutate durable conversation history. Every started provider response is
reconciled as finished, discarded, or active, and adapter source attribution is
added only where authoritative metadata exists; unmatched provider totals stay
in `other`. Legacy `usage.updated` events remain accepted.

The actual production authority catalog currently exposes `FullAccess` only.
Narrower authority values exist in the protocol and UI vocabulary but are not
advertised by the real adapter until they can retain their correct enforcement
meaning.

### Attachments and prompt documents

- PNG, JPEG, GIF, and WebP image attachments.
- MIME sniffing and byte limits.
- Paste, drop, picker, and attachment-only submission.
- Private persistent attachment storage with count/byte reservations that remain
  correct under concurrent ingest.
- Thumbnails and authenticated retrieval.
- Native image input for models that support it.
- Bounded UTF-8 text, Markdown, and ordinary PDF document ingest with immutable
  extraction provenance, hostile-input limits, private storage, and explicit
  prompt-context selection.
- Immutable trusted project-file snapshots selected by opaque file ID.

Audio and other media attachment types are not implemented by the production
host.

### Sources, changes, and outputs

The current checkpoint includes a durable evidence store in
`extensions/ygg-serve/src/resource.rs`:

- immutable evidence blobs;
- versioned metadata and binding records;
- commit manifests;
- SHA-256 integrity verification;
- restart recovery;
- session scoping;
- quotas and bounded reads;
- symlink and path defenses;
- rollback for partial commits;
- exact source content;
- actual unified diffs;
- post-change file snapshots; and
- intentional artifact promotion for newly created Site, Document,
  Spreadsheet, and Presentation outputs.

This closes an important part of the earlier audit, where resources were
process-local and diffs contained only addition and deletion counts.

Coverage is still deliberately narrow. Deterministic evidence currently comes
from successful built-in `read`, `read_skill_resource`, `edit`, and `write`
operations. General Bash or extension mutations, delete and rename, binary
changes, web provenance, and arbitrary tool ecosystems are not captured
comprehensively.

### Frontend

- Project-grouped Tasks sidebar with title-only rows and evidence-gated PR
  marks, plus task-title/preview and transcript search.
- Exception-driven command center with aggregate status, search, priority triage,
  focused-task handoff, and a durable `/overview` route.
- One opaque neutral workbench visual system with semantic status colors and
  borderless tonal pane separation.
- Transcript with Markdown, GitHub-Flavored Markdown, concise collapsed work
  summaries, a separate live item, and expandable typed detail.
- Source, diff, output, and image openers.
- Approval and input interactions.
- Compact run outcome, changed-file, and elapsed-time presentation.
- Pane-width composer with attachments, context, model, effort, authority,
  follow-up/steer, and send/stop.
- Resizable desktop Activity and Inspector panes with full-surface mobile
  overlays.
- Bundled Local Grotesk and Local Mono typography by default with a two-size
  interface/metadata scale and popular device-local font and size alternatives.
- A blue reasoning slider for ordinary effort, locally floating varied white
  particles for exact `xhigh` and `max`, an animated rainbow reserved for exact
  `max`, and static, particle-free reduced motion.
- Settings, review, command history, progress, artifacts, repository context,
  and authoritative context/compaction inspection.
- Project trust/default/archive management and active/archive/trash task
  navigation with guarded permanent deletion.
- A retained, bounded PTY terminal panel when the host advertises process
  execution authority.
- Responsive desktop, tablet, and phone layouts.

The earlier fixture response:

```text
Request understood
Inspected the project context
```

was not a real agent response. Fixture sessions now display an explicit
simulated-data banner, and a production-build assertion prevents fixture
transport from becoming reachable as production behavior.

## What remains fixture-only, specified, or absent

### Projects and context

- The project model is a real durable private registry, not a synthetic
  launch-workspace row. It supports multiple opaque project IDs, one canonical
  root per project, explicit trust, defaults, archive, and durable session
  bindings.
- The loopback browser cannot mint filesystem authority, so it cannot import an
  arbitrary folder. Launching the host for a workspace registers that real
  root; a future host-native picker must supply one-use opaque candidates.
- There is no multi-root project or project-scoped extension-set model, and an
  archived project has no restore UI yet.
- Authenticated, bounded transcript-content search is implemented alongside
  client-side title/preview filtering.
- There is still no tags UI or general session import workflow.
- There is no global command palette; composer `/` discovery is the implemented
  command surface.
- There is no general deep-link system beyond sessions and branches.

### Session semantics

- Edit, retry, conversation fork, and session fork are implemented only at
  validated idle/committed boundaries; there is still no filesystem rollback
  tied to conversation checkout.
- Bounded independent text and attachment drafts persist per host/session in
  browser storage and clear only after an acknowledged submission.
- Accepted queued follow-ups are not durable across a host restart and cannot be
  fully edited, removed, or reordered.
- Provider retries are not shown with attempt count, delay, and sanitized
  cause.
- Context and compaction lifecycle is now projected live and replayed within a
  host run, but the operational tracker is intentionally not conversation
  persistence.
- Structured PR state is produced from bounded `gh` JSON after admitted runs,
  persisted in a Serve-owned sidecar, refreshed for hosted and inventory-only
  sessions, and projected through live session/catalog events. Temporary lookup
  failures retain prior valid evidence; authoritative closure removes it.
- Structured plans exist in DTOs and fixtures, but the real adapter does not
  produce them.

### Outputs and previews

- Production advertises `previews: false`.
- The visible site preview in fixture mode is not a registered production
  live-service preview.
- There is no generalized filesystem change watcher.
- There is no artifact library, output version history, rename/delete
  workflow, or generalized preview sandbox.

### Extensions and agent ecosystem

The web composer has limited, session-scoped parity with TUI resource discovery:

- it exposes only skills, prompt templates, and executable extension commands
  already admitted by the host's existing trust policy;
- `/skills` accepts the TUI list, show, active, search, load, reload, and off
  workflow, with selectable skill names from the discovery payload;
- `/reload`, `/skills reload`, and `/extensions reload` rebuild dynamic
  instructions, prompts, skills, and enabled extensions at an idle boundary;
  and
- extension commands that need an interactive extension confirmation are denied,
  because the web host does not yet expose a confirmation bridge or an extension
  output panel.

The first web release still excludes:

- MCP management;
- a plugin or extension catalog and lifecycle UI;
- extension diagnostics and a dedicated extension-output surface;
- LSP;
- scheduling;
- TUI synchronization; and
- child-agent runtime trees.

The production host advertises `terminal: true` only when configured authority
allows process execution. The terminal is a bounded retained local PTY whose
WebSocket authority is derived from the authenticated page origin.
`childAgents` remains false; the UI does not fake child-agent state.

### LAN and native

The LAN direction is specified in [LAN pairing](lan-pairing.md), but it is not
implemented.

The settled model is:

- accountless and Syncthing-like;
- LAN-only for v1;
- explicit pairing with no automatic LAN trust;
- stable host and device identities;
- a human-verifiable QR and fingerprint ceremony;
- TLS 1.3 with a host-local CA;
- a client-pinned host CA;
- per-device 256-bit credentials;
- a revocable trusted-device registry;
- one authoritative host;
- companion clients rather than replicated agents; and
- deferred WAN, rendezvous, NAT traversal, and relays.

Current production capability flags remain false for Connected Devices and LAN
clients.

The future [native delivery](native-delivery.md) plan uses the shared React
client inside thin system-webview shells. Tauri 2 is the leading candidate;
Electron is out.

There are currently:

- no Tauri project;
- no Xcode or iOS project;
- no Android project;
- no Developer ID signing;
- no notarization;
- no provisioning profiles;
- no TestFlight build; and
- no signed APK or Android App Bundle.

## Validation evidence

The final hardening matrix was run with locked dependencies. Web checks used the
pinned `apps/web/.node-version` runtime, Node `v22.13.0`.

| Gate | Result |
| --- | --- |
| Web install | `npm ci` passed with zero reported vulnerabilities |
| Web lint, typecheck, typography, production build, same-origin/CSP audit, and embedded-bundle check | Pass |
| Web unit tests | Full Vitest suite passed |
| Fixture Playwright matrix | Every applicable test passed for desktop, tablet landscape, tablet portrait, mobile, and mobile-small |
| Production-host Playwright | 1/1 passed against the real Rust host and a disposable local OpenAI-compatible provider; authentication/model selection, streaming, tool replay, `429`/`408` retries, explicit compaction, restart/resume, cancellation, and secret-safe failure projection are covered |
| `ygg-agent` tests | 219 library and 64 agent-run integration tests passed |
| Coding-agent tests | 753 passed with `serve`; 671 passed with default features |
| Full Rust workspace tests | All targets/all features and documentation tests passed |
| No-default-feature workspace check | Pass |
| Independent `extensions/ygg-serve` tests | 115 library tests and every integration suite passed |
| Strict workspace and independent-extension Clippy | Pass |
| Rust 1.86 workspace and independent-extension checks | Pass |
| Rust formatting and `git diff --check` | Pass |
| Package-boundary script | Pass |
| Publishable core workspace package assembly | `cargo +1.90.0 package --workspace --exclude ygg-coding-agent --locked --no-verify` passed from the clean `v0.4.0` candidate tree; Cargo 1.90 is used only for interdependent package assembly, while compilation remains on the Rust 1.86 MSRV |
| Installed `ygg-coding-agent --features serve` smoke | Pass; installed `ygg 0.4.0` served the synchronized embedded bundle |
| Optimized feature-enabled build and bundle smoke | Pass locally with the release binary; no signed tag artifact published |
| Reproducible Serve archive and package dispatch | Two optimized Apple-silicon archives were byte-identical; local install, list, package-dispatched launch, embedded-bundle verification, removal, and data preservation passed |
| Optimized signed serve release | Workflow defined; not yet run against a release tag |

`lopdf` is pinned to `0.42.0`; the independent serve manifest and lockfile are
kept explicit so the PDF parser remains on the audited version. Serve retains
its own strict PDF header, envelope, classic-xref, revision, size, object, and
nesting limits around that parser. A hostile-input regression constructs 4,096
direct nesting levels and verifies Ygg's iterative preflight rejects the input at
its 64-level bound before `lopdf` parsing.

The fixture matrix was split by configured Playwright project after the combined
165-test invocation exceeded the command runner's 120-second limit; no test
failure caused that timeout. Every project then passed independently under the
pinned Node runtime.

The production-host Playwright test uses the real Rust host, real session
adapter, and real provider request path with a disposable local
OpenAI-compatible provider. It now covers the configured-provider conformance
scenarios listed above without inheriting external credentials. Credentialed
checks remain separately protected but are temporarily optional for stable
releases; their supported routes, required environment variables, handling
rules, and current `v0.4.0` waiver are recorded in
[configured-provider acceptance](provider-acceptance.md).

The synchronized embedded bundle has SHA-256:

```text
bc411e451925a63ec17926db70d5a9cf1717d3168cee6deeff3751d2420cc59a
```

## Repository checkpoint

- Repository: `skaft-software/ygg`
- Experimental branch: `explore/ygg-serve-web-v2`
- Pre-hardening branch tip and forward boundary checkpoint:
  `eebe7389097cdcf27cc22b26da75b57a06e4e8e8`.
- This hardening pass remains an uncommitted working-tree change; no merge,
  commit, or push is implied.
- The separate main checkout is intentionally untouched.
- The rejected frontend remains separately archived.

## Package boundary

The intended boundary still keeps the web product in `apps/web`, the optional
backend in `extensions/ygg-serve`, and only narrow generic seams in core crates.
The old default comparison against `c6ec60f` is not meaningful for this stacked
branch: it predates later unrelated core/TUI merges and reports dozens of files.
The earlier documented count of seven violations was therefore stale. Adding
all of those historical paths to an allowlist would conceal rather than enforce
the boundary.

`scripts/check-ygg-serve-boundaries.sh` now uses `eebe738` as an explicit
forward-enforcement checkpoint. It requires the selected base to be an ancestor
of `HEAD` and admits only:

- the application, optional extension, integration adapter, and their owned
  documentation/build paths;
- generic agent-owned context accounting in
  `crates/ygg-agent/src/{agent,context,lib}.rs` and its agent-run tests;
- generic coding-agent configuration diagnostics in `config.rs`,
  `resource_resolver.rs`, and `resources.rs`; and
- the generic primary-session deletion primitive in `session_store.rs`.

The default gate passes for this hardening work. An explicit audit from the old
base still fails on the unrelated/historical paths, so the new baseline does
not silently bless them. This is a forward delta gate, not a claim that the
branch's entire pre-checkpoint history is boundary-clean. Before merge, the
branch still needs comparison or rebase against its actual target and an
explicit review of any surviving pre-checkpoint core delta.

## Visual truth

The frontend now presents one coherent coding-workbench composition rather than
another chat-product skin:

- A 296px project/session sidebar, broad transcript, and optional 400px evidence
  pane establish the desktop hierarchy.
- Headers, user turns, action groups, composer, Activity, and Inspector use
  neutral opaque shades instead of warm tint or structural divider lines.
- Model color does not drive navigation state or composer chrome. Session rows
  contain only their title and an evidence-gated green or purple PR mark.
- Completed work reduces to a concise action-and-duration summary, prior live
  work stays collapsed, and the current live item remains visible. Commands,
  reasoning, metadata, output, and completion review are disclosed on demand.
- The reasoning control is the deliberate color exception only at exact `max`.
  Other effort levels fill blue slightly behind the thumb; exact `xhigh` and
  exact `max` add varied white particles that float locally. Exact `max` alone
  adds the rainbow. Reduced motion freezes that rainbow and removes particles.
- Interface typography resolves to 14px UI and 12px metadata tokens.
- Fresh, populated, Activity, Inspector, performance, tablet, 390px phone, and
  360px phone states have been inspected in a real browser.
- Checked-in baselines now cover the full desktop shell as well as focused
  performance, completion-review, and mobile-inspector states.

The remaining visual truth is product-data debt rather than another styling
pass: production Activity can be sparse because plans and previews have limited
real producers, project import still lacks a host-native folder picker, and
fixture sessions cannot represent every long-running real-agent shape. The
Activity pane remains user-controlled rather than appearing without structured
evidence.

## Recommended next sequence

1. Rebase or compare the forward-gated delta against the intended merge target
   and review any surviving pre-`eebe738` core changes explicitly.
2. Run the experimental release workflow against a clean `v0.4.0` (or
   later stable) tag and retain the checksum/signature verification output.
3. Run `ygg serve` against a user's real provider and exercise:
   - fresh-session creation;
   - real prompt and streaming;
   - tool activity and context/compaction accounting;
   - image attachment;
   - steer, follow-up, edit, retry, and fork;
   - stop and reconnect;
   - terminal reopen and host shutdown;
   - archive, trash, restore, and permanent deletion; and
   - branch checkout plus source, diff, and output reopening after host restart.
4. Iterate on bugs and high-impact layout discrepancies found during that real
   use.
5. Add a host-native project picker and broaden the generalized
   evidence/preview pipeline.
6. Implement secure LAN pairing.
7. Create thin macOS, iOS, and Android shells only after the web and LAN
   contracts are stable.
