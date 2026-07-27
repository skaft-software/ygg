# Ygg Serve current state and handoff

This document is the durable fresh-context handoff for the experimental
graphical `ygg serve` effort.

## Bottom line

`ygg serve` is a substantial, testable experimental vertical slice backed by
real Ygg sessions. It is not yet a finished Claude/ChatGPT-parity product, and
the LAN/native-client layer is still specification-only.

The immediate release blockers are:

- the Serve package-boundary gate does not yet accept seven committed core
  integration changes;
- some acceptance documentation is ahead of the real adapter producers;
- the current production host has not completed final acceptance against a
  user's actual configured model provider; and
- several Cowork-level product surfaces remain deliberately out of scope or
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
| ChatGPT/Claude visual parity | Improved substantially; still visibly short of parity |
| Ready to merge | No |
| Ready for experimental user testing | Yes |

## Settled product

The settled product is not "Ygg Workbench" as a separate mode. It is simply
graphical `ygg`:

- `ygg serve` runs Ygg headlessly.
- There is one normal interaction mode, broadly equivalent to a cowork/work
  interface.
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

The replacement direction used the installed ChatGPT and Claude desktop
applications as behavioral and visual references:

- ChatGPT was inspected as an Electron application containing its `app.asar`,
  the `codex` CLI, and `codex-code-mode-host`.
- Claude was inspected as an Electron application containing its `app.asar`
  and bundled frontend resources.

No proprietary vendor bundle or asset was copied into Ygg. The implementation
is clean-room React and Rust code. The intent is nevertheless deliberately
familiar: ChatGPT/Claude composition, proportions, pacing, and interaction
grammar, subtly restyled as `ygg`.

## Settled visual and interaction language

The UI iterations converged on these rules:

- Always spell the product `ygg`.
- Do not box the logo or put a tiny logo inside another rounded square.
- Do not place the logo next to `ygg` in the sidebar header.
- The model-colored braille tree belongs in the fresh-session splash only.
- The splash derives from the TUI animation, is taller and less compressed,
  and respects reduced-motion settings.
- Do not display fake account, machine, or "connected" footers.
- Connected Devices appears only when the host advertises that capability.
- Use ChatGPT/Claude sidebar pacing: compact rows, quiet headings, small status
  indicators, and restrained selection surfaces.
- Working sessions use a small model-colored loader.
- Attention or unread state uses a small blue dot.
- Avoid count badges and decorative status pills.
- Use shaded surfaces and spacing instead of thin divider outlines.
- Keep shadows restrained and native-feeling.
- Follow the operating system's light or dark appearance.
- Use Geist Sans and Geist Mono by default. IBM Plex, JetBrains Nerd, Iosevka,
  Fira Code, and native system choices remain locally selectable.
- Keep prose in sans-serif. Use monospace only for code, paths, diffs,
  commands, and technical metadata.
- The default-theme composer has no permanent white outline. It is a rounded,
  shaded surface.
- During a run, a model-colored highlight travels continuously around the
  composer's rounded perimeter.
- Stop replaces Send with one pure-white circle containing one dark square.
- The default model picker presents a simple model and effort abstraction.
- Advanced exposes precise model and effort controls.
- There is no speed control.
- The top effort is Max, not Ultra.
- Max uses a slowly moving full-spectrum rainbow, sparse pure-white particles,
  a pure-white thumb, no rainbow thumb ring, and no excessive glow.
- Slider dragging uses local state so an older host value cannot pull the
  control backward.
- Changing the model must not change the application color scheme.
- Do not duplicate Working or Activity indicators.
- The optional right rail is limited to Progress, Artifacts, and Context.
- A dominant inspector opens source, output, image, and diff content.
- Mobile displays one primary surface at a time.

[Web acceptance](web-acceptance.md) still contains an earlier "exactly two
font-size tokens" requirement. Later design feedback intentionally moved the
interface toward a measured ChatGPT/Claude hierarchy. That acceptance item
needs reconciliation.

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

- Sidebar with pinned and recent sessions and client-side title/preview search.
- Transcript with Markdown and GitHub-Flavored Markdown.
- User and assistant messages with grouped activity.
- Source, diff, and output openers.
- Image preview.
- Approval and input interactions.
- Compact run outcome and elapsed-time presentation.
- Composer with attachments, model, effort, authority, follow-up/steer, and
  send/stop.
- Ten projected Ygg themes.
- Operating-system appearance support.
- Device-local font and size preferences.
- Settings, inspector, progress, artifacts, and context.
- Responsive desktop, tablet, and phone layouts.
- TUI-derived animated braille splash.

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

The checkpoint was validated with the following results:

| Gate | Result |
| --- | --- |
| Web typecheck | Pass |
| ESLint | Pass |
| Production frontend build | Pass |
| Fixture/production boundary check | Pass |
| External request and CSP policy | Pass |
| Font policy | Pass |
| Embedded bundle synchronization and integrity | Pass |
| Web unit tests | 89/89 pass on the final run |
| Fixture Playwright matrix | 68 pass, 42 intentional skips |
| Production-host Playwright | 1/1 pass |
| `ygg-serve` Rust unit tests | 58 pass |
| Golden protocol tests | 6 pass |
| Coding-agent tests with `serve` | 624 unit and 9 SIGTERM pass |
| Strict Clippy | Pass |
| Rust formatting | Pass |
| `git diff --check` | Pass |
| Exact-feature binary build | Pass |
| Installed-binary embedded-web smoke | Pass |
| Package-boundary gate | **Fail** |

Earlier web-unit runs exposed a focus assertion that executed before the
composer menu's scheduled animation-frame focus restoration. The checkpoint
now waits for that asynchronous accessibility contract rather than making an
immediate assertion.

The production-host Playwright test uses the real Rust host, real session
adapter, and real provider request path with a deterministic local
OpenAI-compatible provider. It proves the integration without requiring an
external model. Final acceptance against a user's actual configured provider
remains outstanding.

The embedded bundle at this checkpoint has SHA-256:

```text
6150a6359334797750e45c958715e29025368cb513aa89e7e1d05d7e1f1ad753
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

The current UI is much closer than the rejected dashboard, but recent
side-by-side inspection still shows clear differences from ChatGPT:

- Ygg is sparser vertically and leaves more dead space.
- The light theme is warmer and more beige than OpenAI's neutral white.
- Transcript composition is less dense and editorial.
- The sidebar has fewer navigation anchors and less mature pacing.
- The advanced model and effort popover still feels detached and somewhat
  oversized.
- The composer is less integrated into the conversation's visual rhythm.
- The right rail often disappears because production lacks real preview and
  plan producers.
- Fixtures do not naturally display the richness of a long real agent session.
- Small control weights, icon sizing, bubbles, and alignment remain a polish
  pass short of a native application.

That is iteration debt, not a reason to delay experimental testing.

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
