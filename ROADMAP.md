# Ygg roadmap

Ygg is building a small, local-first coding-agent kernel with strict boundaries for
models, sessions, tools, and language-neutral extensions.

The long-term direction is broader than a plugin list:

> Models are interchangeable brains. Extensions are senses, hands, instruments,
> and institutions. Ygg is the language-neutral nervous system connecting them.

The near-term product promise is narrower and testable:

> One agent can operate across existing software, services, runtimes, and devices
> without requiring those ecosystems to be rewritten in JavaScript.

This roadmap describes outcomes and boundaries, not dates. Items are candidates
until accepted into a milestone. Scope and order may change as evidence arrives.
[Release notes](CHANGELOG.md) are authoritative for shipped behavior.

- **Live work:** [public roadmap project](https://github.com/orgs/skaft-software/projects/5)
- **Prioritization:** [pinned roadmap issue](https://github.com/skaft-software/ygg/issues/198)
- **Release buckets:** [milestones](https://github.com/skaft-software/ygg/milestones)
- **Proposals:** [Discussions](https://github.com/skaft-software/ygg/discussions)
- **Security:** [report privately](SECURITY.md)

## How to read the roadmap

- **Exploring:** worth investigating; not a commitment.
- **Accepted / Next:** accepted into a milestone, but not actively implemented.
- **In progress:** actively owned implementation.
- **In review:** implementation exists and is being validated.
- **Shipped:** released and evidenced.

A conceptual proposal can be accepted for discussion without being accepted for
implementation. Large changes should state the user problem, smallest complete
outcome, core-versus-extension placement, non-goals, success measure, and a reason
to stop.

## Engineering principles

### Keep contracts small

- Core owns canonical model, session, operation, effect, cancellation, resource,
  and presentation contracts.
- Domain behavior belongs in extensions or declarative definitions.
- First-party packages receive no hidden policy authority.
- Unsupported behavior fails explicitly; silent compatibility is forbidden.

### Prefer data and generation to branches

- Provider and model inventories should be generated or declarative.
- New provider code is justified by a genuinely new protocol, authentication
  primitive, transport, or safety boundary—not another provider name.
- Protocol schemas, SDK models, fixtures, and reference tables should share one
  structural source of truth.
- Compatibility shims stay narrow, typed, attributable, and tested.

### Refactor in service of ownership

Ygg is not currently three million lines of live source. Current public `main` is
roughly 484,000 text lines, including tests, generated/vendor files, benchmarks,
documentation, and configuration. The maintenance problem is nevertheless real:
several handwritten production modules are 5,000–17,000 lines and own unrelated
responsibilities.

- No parallel rewrite or permanent `v2` tree.
- Before adding a major capability to a god object, extract the boundary that
  will own it.
- Moving lines without reducing responsibility is not simplification.
- A touched monolith must become smaller or delegate to cohesive modules.
- Generated, vendor, test, fixture, and handwritten production code are measured
  separately.
- By v0.8, every handwritten production file above 5,000 lines must be split,
  generated, or explicitly justified.

Tracking: [maintainability umbrella #19](https://github.com/skaft-software/ygg/issues/19).

## Now — v0.6.6 daily-driver closure

[v0.6.6 milestone](https://github.com/skaft-software/ygg/milestone/7) ·
[parent epic #186](https://github.com/skaft-software/ygg/issues/186)

This is the final non-breaking v0.6 release. It finishes the product already in
users' hands; it does not absorb v0.7 runtime work.

### Terminal trust

- Remove visible flicker during typing, streaming, run transitions, steering,
  interruption, resize, and panel changes.
- Fix long-transcript/native-scroll corruption.
- Give status, context, cost, cache, extension setup/status, provider setup,
  pickers, confirmations, and errors one coherent chrome/layout contract.
- Print one exact `ygg --resume <session-id>` command after a clean TUI exit.
- Add deterministic ANSI/VT100/PTY performance and correctness traces.

Tracking: [TUI closure #187](https://github.com/skaft-software/ygg/issues/187).

### Reliability and repository safety

- Close or explicitly waive supported-path security, data-integrity,
  cancellation, process-tree, unbounded-resource, and cross-session defects.
- Protect `main` with pull requests, required checks, conversation resolution,
  and no force-push/delete.
- Enable private vulnerability reporting, secret scanning/push protection,
  dependency security updates, and code scanning.
- Remove private research, raw runtime homes/sessions/caches, abandoned clone
  themes, machine-local paths, and stale current-state artifacts.
- Keep compact reproducible benchmark evidence and required provenance.
- Preserve and classify every user-submitted issue; external reports are not
  closed merely to make the backlog look smaller.

Tracking: [repository safety #188](https://github.com/skaft-software/ygg/issues/188) ·
[cleanup #197](https://github.com/skaft-software/ygg/issues/197).

### Release and distribution

- Promote one protected immutable candidate through build, signing, publication,
  and post-publish verification.
- Preserve reproducible GitHub release archives, checksums, signatures,
  attestations, installer smokes, and repair procedures.
- Add npm trusted publishing with native platform packages and no network-running
  postinstall hook.
- Add a Homebrew tap/formula with audit/install/update/uninstall tests.
- Verify GitHub, npm, and Homebrew resolve the same version and provenance.

Tracking: [release/distribution #189](https://github.com/skaft-software/ygg/issues/189).

### Exit gate

- No known unclassified critical vulnerability or live secret.
- Required release, security, package, installer, and post-publish checks pass on
  the exact candidate.
- Canonical TUI traces show no corruption or unexplained destructive replay.
- Public docs and package versions agree.
- An unguided beta covers install, first task, clean exit/resume, cancellation,
  and second-day return.

## Next — v0.7 universal runtime and pinned Pi parity

[v0.7 milestone](https://github.com/skaft-software/ygg/milestone/8) ·
[parent epic #190](https://github.com/skaft-software/ygg/issues/190)

v0.7 is a breaking runtime release. It must not merely append API 0.3 to existing
god objects.

### Runtime ownership and activation

- One extension runtime manager per ordinary host; one per Serve host partitioned
  by canonical workspace and explicit trust domains.
- `App` holds a session binding, not the process fleet.
- Static content-bound catalogs make enabled extensions eligible for activation
  without starting every process.
- Explicit lifecycle profiles cover legacy resident, lazy resident, oneshot,
  session, workspace service, always, and Pi aggregate behavior.
- Sharing is explicit and content-digested, never inferred from language.
- Aggregate FD/process/byte/startup/reload/restart governance returns visible
  `resource_exhausted` failures instead of silently omitting capabilities.

### Provider-neutral runtime

- Separate provider definition, authentication lifecycle, credential storage,
  model catalog, compatibility metadata, pricing/quota, and request runtime.
- Migrate existing providers to the contract before expanding breadth.
- Express providers using existing OpenAI Chat, Responses, or Anthropic codecs as
  declarations plus bounded compatibility data.
- Add a new codec only for a genuinely new API family.
- Keep subscription quota/availability distinct from notional token cost.

### Pi compatibility

- Preserve API 0.1/0.2 as documented legacy contracts.
- Ship API 0.3 on the decomposed runtime boundary.
- Keep all selected Pi sources in one exact, ordered, fingerprinted real Pi
  `ExtensionRunner` process.
- Complete pinned Pi 0.84.4 public-extension parity: plan mode, all 78 official
  extension examples, 33 TUI audit rows, provider/OAuth paths, and zero silent
  unsupported calls.
- Provide inventory → plan → inert publication → preflight/handshake → explicit
  enable/trust → rollback as one understandable migration journey.
- Generate/check protocol fixtures and Python/TypeScript SDKs from one schema.

### Release gate

- 100 eligible lazy extensions under a soft FD limit of 256 do not exhaust the
  host or disappear silently.
- Compatible App/session rebuilds do not restart workspace-safe runtimes.
- Providers expressible with existing codecs require no new core provider-name
  branches.
- No-extension startup, RSS, CPU, FD, first-activation, warm-call, reload, and
  multi-session results are published on macOS and Linux.
- Extension and provider god objects are decomposed before release.

## Evidence and public launch

[Launch milestone](https://github.com/skaft-software/ygg/milestone/10)

This lane can prepare in parallel, but public claims use only released, immutable
builds. The ambition is to beat top comparable performers. The commitment is to
freeze valid methodology and publish the observed result, including losses.

### Immediate website lane

Publish the new product/docs site as soon as the name, install path, security
boundary, and canonical roadmap are stable. The first viewport should state the
language-neutral capability thesis, scoped evidence, and install action without
an unsupported universal speed claim.

Tracking: [website #193](https://github.com/skaft-software/ygg/issues/193).

### Measured systems footprint

Publish repeated median/p95 cold readiness, settled and peak root-plus-descendant
RSS/PSS, CPU, child/thread/FD counts, concurrency, resume, first activation, and
warm-call results. Separate agent overhead from inference-server resources.

Tracking: [peak RSS/process footprint #191](https://github.com/skaft-software/ygg/issues/191).

### Terminal-Bench 2.1 and Harbor Index

Run a pre-registered, pinned, reproducible campaign and submit through the
accepted Harbor Index path when eligible. Preserve all trials, failures, costs,
trajectories, hashes, exclusion sensitivity, and official-versus-local audit
scope. Update public placement claims only after adjudication settles.

Tracking: [TB2.1/Harbor campaign #192](https://github.com/skaft-software/ygg/issues/192).

### Terminal-Bench 4

Begin retained trials only after TB4's dataset, adapter, scoring, and submission
contract are stable enough to pin. Reuse the evidence/failure discipline above;
do not add hidden benchmark-specific product behavior.

Tracking: [TB4 campaign #194](https://github.com/skaft-software/ygg/issues/194).

### First adoption gate

- 25 independent users install Ygg and complete a real repository task.
- At least 10 return within seven days.
- At least 5 use Ygg on three separate days within 14 days.
- Publish denominators, exclusions, abandonment reasons, and representative
  criticism without requiring telemetry or private user data.

Tracking: [first 25 users #195](https://github.com/skaft-software/ygg/issues/195).

## After — v0.8 Serve orchestration

[v0.8 milestone](https://github.com/skaft-software/ygg/milestone/9) ·
[parent epic #196](https://github.com/skaft-software/ygg/issues/196)

Outcome: one operator can supervise many independent repository tasks, notice
exceptions immediately, intervene safely, and move completed work through review
and PR workflows from a quiet, fast interface.

- One durable mutable owner per root session; clients attach rather than own work.
- Bounded queueing, budgets, cancellation, pause/resume, steering, approvals,
  inputs, reconnect, and explicit recoverable terminal states.
- Root tasks remain primary; child agents are inline/on-demand evidence.
- Exception-first Overview → focused Task → Review/PR hierarchy.
- Structured branch/PR/check/review state comes from trusted sources, not model
  prose.
- Consequential remote actions remain explicit and confirmation/policy gated.
- Hundreds of dormant sessions remain catalog rows; active work alone consumes
  runtime resources.

Non-goals include a generic graph canvas/runtime, parallel writers, automatic
worktree merging, autonomous push/merge, hosted control plane, distributed
scheduler, and a persistent multi-agent pane in the ordinary TUI.

## Exploring — not commitments

- Linux containment using cgroup v2, Landlock, seccomp, no-new-privs, rlimits,
  closed inherited FDs, and brokered egress.
- WASM density tier and explicit same-trust language pools.
- Hardware/device hot-plug catalogs with external safety interlocks.
- Scientific executable publications, accessibility hardware, legacy industrial
  systems, environmental sensing, and artistic instruments.
- Thin native/LAN companion clients after Serve's host/client protocol settles.
- Additional pinned Pi profiles and migration recipes.

Ygg is not currently an industrial safety kernel. Safety-critical physical
actions require external interlocks, human control, and a separately reviewed
safety architecture.

## Contributing to priorities

User reports are product evidence, not backlog noise. Existing external reports
remain classified in milestones or Exploring, and closure requires shipped
evidence, a verified duplicate, or an explicit product/non-goal decision.

For a new capability, start in [Discussions](https://github.com/skaft-software/ygg/discussions)
or the matching issue form. For a vulnerability, stop and follow
[SECURITY.md](SECURITY.md); do not open a public issue.
