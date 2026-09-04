# ygg-mcp changelog

## Unreleased

- Add an explicit, bounded Streamable HTTP transport with negotiated session
  identity, JSON/SSE response framing, no-replay SSE resumption, cancellation,
  status/content-type policy, and lifecycle reconnects.
- Add strict remote configuration, exact-origin/TLS/redirect controls, and a
  non-persistent bearer credential-adapter boundary; OAuth and static credential
  configuration remain intentionally unsupported.

## 0.1.0 — Ygg 0.6.1

- Add the first API `0.2` dynamic-catalog MCP bridge.
- Support explicit user and digest-pinned trusted-project configuration for
  bounded local stdio tool servers, with private permissions required whenever
  explicit environment values are present.
- Add epoch-aware add/replace/remove publication, conservative approval,
  cancellation, timeout, reconnect/backoff/parking, bounded logs, and graceful
  cleanup.
- Preserve MCP text, structured, image, and audio results through Ygg's typed
  result and artifact boundaries.
- Publish the generic frontend-neutral status/activity/tree/detail/action
  snapshot used by TUI and Serve, with headless `/mcp` fallbacks.
- Bundle the tested dependency-free Python SDK, real/adversarial fixtures,
  presentation fixtures, release smoke coverage, and configuration schema.
