# Extension capability and orchestration boundaries

This document defines how Ygg assigns capability ownership across host, extension, and delegated execution. It is the non-goals and ownership artifact for issue `#142`.

> Normative scope: non-`ygg-serve` backlog planning and extension-capability work. It does **not** revise protocol contracts.

## Capability classes and ownership

## 1) Web search/retrieval

- **Owner:** Ygg extension or external hosted service invoked by an extension (for example an extension-owned web search tool).
- **Input/observability:** query string and bounded result payload (snippets, title, url, score, citation metadata).
- **Orchestration:** Ygg does not coordinate search result interpretation. The caller extension invokes retrieval and formats prompts/arguments for the agent model.
- **State model:** usually stateless per request; no notion of tabs or persisted browsing sessions is implied by search retrieval.
- **Trust boundary:** Search results are untrusted text. They should be bounded, redacted from secrets, and never used to relax tool/policy semantics.

## 2) Browser control (DOM/tooling)

- **Owner:** extension-owned browser automation capability.
- **Input/observability:** DOM actions, navigation/events, accessibility reads, and bounded screenshot/DOM extracts.
- **Orchestration:** Ygg owns the child process lifecycle only when using executable extensions; browser behavior, profile lifecycle, and navigation strategy remain extension-owned.
- **State model:** extension-owned long-lived browser state (session/profile/tab/window IDs) scoped by host-provided resource owner context.
- **Trust boundary:** Browser credentials or page content must stay bounded and redacted in telemetry and prompts. Browser control is **not** interchangeable with desktop “computer use,” and should not be used to grant host privileges beyond the owning extension’s declared capabilities.

## 3) Computer use (desktop-level automation)

- **Owner:** extension-owned host-specific desktop control surface.
- **Input/observability:** pointer/keyboard events, screenshot deltas, and app-level state from accessibility hooks.
- **Orchestration:** desktop interactions are scheduled and interpreted by the owning extension process; Ygg provides generic process and artifact/kernel services only.
- **State model:** per-extension/owner session state, not tied to web browser tab state.
- **Trust boundary:** this is a separate capability class from web browser control and from hosted retrieval workflows.

## 4) Delegated execution path

### 4a) Server-hosted delegation

- **Owner of scheduling:** external provider (server-side subagent APIs).
- **Orchestration boundary:** the provider selects concrete subagent lifecycle,
  budgets, and completion policy; Ygg must not enable an unobservable provider
  subagent tier.
- **State model:** results are returned as provider response artifacts; there is
  no shared local tool session tree owned by Ygg.
- **Failure and cancellation:** failure taxonomy is governed by provider
  semantics. Ygg cannot infer local process-level lifecycle details beyond
  provider responses.

### 4b) In-harness delegated children

- **Owner of user-facing orchestration:** the `ygg-subagents` extension. It is
  the required observation and control surface for every coding-product child,
  regardless of whether the child was requested by a model tool or another
  local frontend.
- **Kernel boundary:** `ygg-agent` owns child `Agent` sessions, mailboxing,
  persistence, ancestry trees, resource limits, and lifecycle cancellation
  behind the extension's owner-bound `agent_sessions` service.
- **State model:** each child has its own append-only session and durable
  delegation state; child runs inherit the parent’s approval, sandbox policy,
  model/reasoning settings, compaction policy, and output/turn policy.
- **Failure and cancellation:** parent outcomes and shutdown requests propagate
  through delegation tokens and cancellation paths; child failures do not
  terminate sibling trees unless resource/queue policy requires.
- **Resource ownership:** artifact/resource handles and resource-owner tracing
  remain keyed through the owning parent context.
- **Safety gate:** Ultra is not selectable unless the trusted, enabled
  `ygg-subagents` extension has successfully negotiated its observation service.
  The coding host does not expose a parallel native root collaboration surface.

## Trust and policy inheritance for extensions and children

For non-`ygg-serve` extension workflows, Ygg keeps ownership of policy and lifecycle while the extension owns domain behavior:

- **Extensions are not trusted for policy authority.** They receive capability declarations, session context, and bounded broker services, but do not lower `EffectPolicy`.
- **Extension trust is explicit.** Running executable extensions requires explicit enablement and explicit trust grants in addition to run policy gates.
- **In-harness delegation inherits parent boundaries.** Delegated children inherit the parent’s sandbox, approvals, extension selection, cache/resolution policy, environment/cwd model, and budget constraints.
- **No implicit process isolation from trust alone.** All extension and delegated work still uses the host’s existing security model unless containment is added outside Ygg.
- **Telemetry redaction constraints:** host/extension telemetry should carry bounded, non-secret fields; capability/provenance metadata (delegation kind, owner/request lineage, lifecycle outcome, cancellation source, timeout source) is allowed only where bounded and redacted.

## Explicit non-goals for unimplemented capabilities

These do **not** move into Ygg core unless a separate issue is opened with a distinct contract:

- Ygg kernel implementing browser automation directly.
- Ygg kernel implementing computer-use tooling directly.
- Universal in-kernel graph/runtime orchestration for arbitrary agent topologies (`#119` is a spike with narrow scope and explicit stop criteria).
- File-tree/worktree isolation for delegated children by virtue of delegation alone.
- Automatic trust transfer between hosted and in-harness execution.

## Cross-links to backlog work

- `#73` requires this ownership model when documenting continuation state transitions and anti-spin behavior.
- `#109` and `#110` should emit bounded, capability-aware telemetry/diagnostics under this provenance model.
- `#121` remains extension-owned (executable bundle + skill) and must not imply a core-browser capability.
- `#142` is the originating issue for this boundary text.
- `#133` should continue to rely on shared extension lifecycle contracts (`extensions/*/settle_turn`, terminal lifecycle outcomes) and this ownership boundary.
