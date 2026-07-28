# Ygg Serve current state and handoff

This document is the durable fresh-context handoff for the experimental
graphical `ygg serve` effort.

## Bottom line

`ygg serve` is a substantial, testable experimental vertical slice backed by
real Ygg sessions. It is not yet a finished coding workbench, and the
LAN/native-client layer is still specification-only.

The immediate release blockers are:

- the Serve package-boundary gate does not yet accept seven committed core
  integration changes;
- some acceptance documentation is ahead of the real adapter producers;
- the current production host has not completed final acceptance against a
  user's actual configured model provider; and
- several workbench-level product surfaces remain deliberately out of scope or
  incomplete.

## Current status at a glance

| Area | Reality today |
| --- | --- |
| Real local Ygg agent sessions | Yes |
| Streaming, tools, approvals, stop, steer, follow-up | Yes |
| Multiple independent sessions | Yes |
| Reconnect, replay, resume, and branch checkout | Yes |
| Images and multimodal composer | Images only |
| Sources, diffs, and outputs | Real but limited to specific built-in tools |
| Live generated-site previews | Fixture UI exists; production capability is off |
| Projects and folder management | One synthetic launch workspace only |
| Extension, skills, MCP, or LSP GUI | Not implemented |
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
- There is one normal interaction mode: a repository-oriented coding
  workbench.
- Opening the root creates a fresh provisional session.
- An explicit session route restores an existing session.
- Previous, pinned, and running sessions appear in the sidebar.
- Different sessions are independent agent sessions and may run concurrently.
- There is no Chat, Code, Work, or Cowork mode selector.
- There is no synchronized TUI, terminal mirror, or terminal-window mode.
- A phone is eventually a companion/controller for the host, not a mobile
  agent runtime.
- The transcript is primary. Sources, actions, outputs, previews, and progress
  appear only when structured events justify them.
- There is no account, login, hosted control plane, or telemetry.
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

## Settled visual and interaction language

The UI iterations converged on these rules:

- Always spell the product `ygg`; keep the wordmark unboxed and unaccompanied
  by a decorative logo in the sidebar header.
- Use one opaque, neutral-dark workbench appearance. Do not use glass,
  translucency, or model-driven application colors. Rainbow color is reserved
  for the reasoning-effort control.
- The desktop shell is a real three-pane workbench: a 296px project/session
  sidebar, a broad center pane, and an optional 400px evidence pane. Distinct
  shaded surfaces separate panes without one-pixel divider lines.
- Label navigation as Sessions, retain project grouping, and keep each active
  session row to its title plus an optional PR mark. The PR mark appears only
  for structured `in_progress`, `ready`, or `merged` evidence; repository state,
  model metadata, and generic run status are not synthesized into the row.
- Status color is semantic: green means working or successful, amber means
  attention, and red means failure. Provider and model colors do not determine
  shell state.
- Keep the fresh session quiet: “New workspace task,” “What should we work on?”,
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
- Use native system UI and system monospace by default. The visible type scale
  has two sizes: 14px interface text and 12px metadata. Monospace remains
  limited to code, paths, diffs, commands, and technical metadata. Local font
  and size preferences remain available.
- The model picker keeps the simple model/effort abstraction, with precise
  controls under Advanced. Ordinary effort fills the track in blue through and
  slightly behind the white thumb without a rounded-cap gap. Exact `xhigh` adds
  varied white particles that float locally within the blue fill. Exact `max`
  combines those particles with the animated rainbow and is the only rainbow
  state. Reduced motion freezes that rainbow and removes all particles. Changing
  models never changes the shell appearance.
- Do not duplicate Working or Activity indicators. The optional right pane is
  limited to review, command history, progress, artifacts, and context backed
  by structured events.
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
CLI do not depend on the web surface.

The actual CLI supports:

```text
ygg serve
  --no-open
  --port <u16>
  --web-root <directory>
```

There is no implemented `--lan`, `--demo`, or `--local-only` switch.

From the repository root, the shortest development launch command is:

```console
cargo run --features serve -- serve
```

`--features serve` remains necessary because the graphical host is an opt-in
experiment. `--port 0` may be added to request an ephemeral port.

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
- Explicit session restoration.
- Concurrent independent graphical sessions.
- Exactly one mutable `App` owner per session.
- Rename, pin, and archive.
- Host-authoritative session titles and catalog updates.
- Authenticated, redacted JSON export.
- Branch graph projection and safe idle-boundary branch checkout.

### Agent interaction

- Real prompt submission through the coding-agent path.
- Streaming assistant text and reasoning.
- Structured tool calls, results, and progress.
- Approval and typed-input requests.
- Stop.
- Steering.
- Queued follow-up.
- Durable run outcomes.
- Model catalog and model selection.
- Reasoning-effort selection.

The actual production authority catalog currently exposes `FullAccess` only.
Narrower authority values exist in the protocol and UI vocabulary but are not
advertised by the real adapter until they can retain their correct sandbox
meaning.

### Attachments

- PNG, JPEG, GIF, and WebP.
- MIME sniffing and byte limits.
- Paste, drop, picker, and attachment-only submission.
- Private persistent attachment storage.
- Thumbnails and authenticated retrieval.
- Native image input for models that support it.

Audio, documents, and other protocol modality names are not implemented by the
production host.

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

- Project-grouped Sessions sidebar with title-only rows and evidence-gated PR
  marks, plus client-side title/preview search.
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
- Native system typography by default with a two-size interface/metadata scale
  and device-local font and size preferences.
- A blue reasoning slider for ordinary effort, locally floating varied white
  particles for exact `xhigh` and `max`, an animated rainbow reserved for exact
  `max`, and static, particle-free reduced motion.
- Settings, review, command history, progress, artifacts, and context.
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

- Only one synthetic project derived from the launch workspace.
- No project registry, CRUD, trusted-root picker, multi-folder model, or
  project defaults.
- No project-scoped extension set.
- No host-side full-text transcript search.
- No tags UI, archive browser, restore, delete, duplicate/fork, or import.
- No command palette.
- No general deep-link system beyond sessions and branches.

### Session semantics

- No prior-message edit, retry, or regenerate workflow.
- No fork-to-new-session UI.
- No filesystem rollback tied to conversation checkout.
- Draft and queued-follow-up durability remains incomplete.
- Queued follow-ups cannot be fully edited, removed, or reordered.
- Provider retries are not shown with attempt count, delay, and sanitized
  cause.
- Live `SteeringDelivered`, `CompactionStarted`, and `CompactionFinished`
  events are currently ignored, although durable compaction records can appear
  after hydration.
- Structured PR state is optional in `SessionSummary` and covered by fixtures,
  but production has no PR evidence producer yet and therefore omits it.
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

The first web release intentionally excludes:

- skills management;
- MCP management;
- plugin or extension catalog and lifecycle;
- extension diagnostics and reload;
- LSP;
- scheduling;
- interactive terminal;
- TUI synchronization; and
- child-agent runtime trees.

The production host advertises `terminal: false` and `childAgents: false`. The
UI does not fake either feature.

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

The web and focused `ygg-serve` rows below were rerun after the workbench
visual pass. Production-host and broader coding-agent rows retain the earlier
integration-checkpoint evidence.

| Gate | Result |
| --- | --- |
| Web typecheck | Pass |
| ESLint | Pass |
| Production frontend build | Pass |
| Fixture/production boundary check | Pass |
| External request and CSP policy | Pass |
| Font policy | Pass |
| Embedded bundle synchronization and integrity | Pass |
| Web unit tests | 151/151 pass |
| Fixture Playwright matrix | 74 pass, 70 intentional skips, 1 system-Chrome timing failure |
| Production-host Playwright | 1/1 pass at the integration checkpoint |
| `ygg-serve` Rust tests | 69 library tests and associated suites pass |
| Golden protocol tests | 9/9 pass |
| Coding-agent tests with `serve` | 624 unit and 9 SIGTERM pass at the integration checkpoint |
| Strict Clippy | Pass at the integration checkpoint |
| Rust formatting | Repository-wide check still encounters pre-existing formatting differences outside this change |
| `git diff --check` | Pass |
| Exact-feature binary build | Pass at the integration checkpoint |
| Installed-binary embedded-web smoke | Pass at the integration checkpoint |
| Package-boundary gate | **Fail** |

The installed system Chrome was used because Playwright's configured Chromium
binary is not present locally. Its only full-project failure was the existing
60-delta performance probe: scroll-retention, stream-time, and frame-rate
assertions passed, but Chrome reported one 110ms long task. The unmodified Git
`HEAD` reproduces one or two long tasks under the same browser, so this is not a
regression from the workbench styling. It still needs confirmation with the
locked Playwright Chromium before the matrix can be called fully green.

Earlier web-unit runs exposed a focus assertion that executed before the
composer menu's scheduled animation-frame focus restoration. The checkpoint
now waits for that asynchronous accessibility contract rather than making an
immediate assertion.

The production-host Playwright test uses the real Rust host, real session
adapter, and real provider request path with a deterministic local
OpenAI-compatible provider. It proves the integration without requiring an
external model. Final acceptance against a user's actual configured provider
remains outstanding.

The synchronized embedded bundle after the compact-activity and slider pass has
SHA-256:

```text
6339e9e630069c1adb9f3bd1a430956fe6269a122320bbd54e406d15d0aa9f45
```

## Repository checkpoint

- Repository: `skaft-software/ygg`
- Experimental branch: `explore/ygg-serve-web-v2`
- Parent before this checkpoint: `374a2a9`
- The branch contains 40 earlier Serve commits beyond its merge base.
- The main checkout remained clean and untouched while this checkpoint was
  prepared.
- Main contains two later TUI commits that are not yet incorporated into this
  branch.
- The rejected frontend remains separately archived.

No merge or push is implied by this checkpoint.

## Package-boundary failure

The intended boundary keeps the feature in the extension and app packages with
only a minimal coding-agent seam. The branch currently violates the strict
boundary gate because seven out-of-allowlist core files changed:

- `crates/ygg-agent/src/agent.rs`
- `crates/ygg-agent/src/lib.rs`
- `crates/ygg-agent/src/session.rs`
- `crates/ygg-agent/tests/agent_run.rs`
- `crates/ygg-coding-agent/src/hydrate.rs`
- `crates/ygg-coding-agent/src/session_commands.rs`
- `crates/ygg-coding-agent/src/session_store.rs`

These changes came from:

- `77f7c65`, which persisted run outcomes and corrected steering attribution;
  and
- `237a068`, which persisted pin and archive metadata.

The boundary audit found that one part is a legitimate cross-frontend core
fix: steering and follow-up messages must not inherit the initial prompt's
display text. Without that correction, resumed transcripts can repeat the
first visible prompt for every steering message.

The preferred repair is:

1. Keep the small generic steering-attribution fix as an independently
   justified core bug fix.
2. Move Serve run outcomes into an extension-local
   `.serve/session-state-v1` store.
3. Move pin and archive state into the same extension-owned store.
4. Keep existing core title rename support.
5. Migrate experimental metadata before removing the temporary core schema.
6. Restore the remaining core files and make the boundary gate pass.

A strict zero-core approach is possible, but it requires a more complicated
extension-side prompt-intent mapping and would reintroduce the native steering
attribution bug.

## Visual truth

The frontend now presents one coherent coding-workbench composition rather than
another chat-product skin:

- A 296px project/session sidebar, broad transcript, and optional 400px evidence
  pane establish the desktop hierarchy.
- Headers, user turns, action groups, composer, Activity, and Inspector use
  neutral opaque shades instead of warm tint or structural divider lines.
- Model color does not drive navigation state or composer chrome. Session rows
  contain only their title and an evidence-gated gray, green, or purple PR mark.
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
real producers, the repository model is still synthetic, and fixture sessions
cannot represent every long-running real-agent shape. The Activity pane remains
user-controlled rather than appearing without structured evidence.

## Recommended next sequence

1. Resolve the seven-file package-boundary issue using the extension-local
   state plan.
2. Incorporate the two newer main commits without changing the clean main
   checkout.
3. Run `ygg serve` against a user's real provider and exercise:
   - fresh-session creation;
   - real prompt and streaming;
   - tool activity;
   - image attachment;
   - steer and follow-up;
   - stop;
   - reconnect;
   - branch checkout; and
   - source, diff, and output reopening after host restart.
4. Iterate on bugs and high-impact layout discrepancies found during that real
   use.
5. Build the project and trusted-root model and generalized evidence/preview
   pipeline.
6. Implement secure LAN pairing.
7. Create thin macOS, iOS, and Android shells only after the web and LAN
   contracts are stable.
