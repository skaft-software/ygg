# ygg-mcp changelog

## 0.1.0 — Ygg 0.5.0

- Add the first API `0.2` dynamic-catalog MCP bridge.
- Support explicit user and digest-pinned trusted-project configuration for
  bounded local stdio tool servers.
- Add epoch-aware add/replace/remove publication, conservative approval,
  cancellation, timeout, reconnect/backoff/parking, bounded logs, and graceful
  cleanup.
- Preserve MCP text, structured, image, and audio results through Ygg's typed
  result and artifact boundaries.
- Publish the generic frontend-neutral status/activity/tree/detail/action
  snapshot used by TUI and Serve, with headless `/mcp` fallbacks.
- Bundle the tested dependency-free Python SDK, real/adversarial fixtures,
  presentation fixtures, release smoke coverage, and configuration schema.
