# `ygg-serve` backend experiment

This optional first-party package defines the frontend-neutral backend boundary
for graphical Ygg clients. It deliberately lives outside the shipping Cargo
workspace while the contract is experimental.

The package contains:

- bounded host, project, session, command, item, event, and replay DTOs;
- host-authoritative model, authority, capability, and safe theme catalogs;
- stable session cursors and exact durable entry identity;
- a deterministic session snapshot reducer;
- bounded replay plus device-scoped command idempotency;
- a host-scoped idempotent fresh-session operation;
- a serialized `SessionActor`;
- a `SessionSupervisor` that prevents duplicate mutable owners without holding
  its actor-map lock across slow session factories; and
- a loopback-only HTTP/WebSocket transport with one-use launch authentication,
  strict same-origin checks, bounded requests, and safe static assets;
- an optional bounded in-process PTY manager for authenticated local terminal
  sessions; and
- `HostService` / `SessionDriver` adapter traits for the real Ygg application.

It does **not** contain a TUI, web layout, provider client, Agent, authenticated
LAN pairing, upload/content route, or a second session format. The
feature-gated first-party adapter in `ygg-coding-agent` owns one existing
`App` inside each driver, translates real `AgentEvent` values into
`TimestampedEvent`, and hydrates committed `SessionItem` values from Ygg's
append-only JSONL.

Golden JSON contracts for the browser/native client boundary live in
`fixtures/`. They use camel-case fields and explicit dotted command/event
discriminators.

## Local terminal

When the host's process-execution sandbox permission is enabled, the loopback
transport exposes an authenticated same-origin terminal WebSocket. It starts
shells only in the configured workspace, retains at most four sessions, and
uses an opaque owner key to reattach after a browser disconnect. Replay, input,
and terminal dimensions are bounded. Browser detach retains a shell; loopback
server shutdown stops every retained shell.

## Core adapter requirements

The eventual Ygg adapter must:

1. Create or open exactly one `App`/`Agent`/`Session` per driver.
2. Keep at most one active `Run` inside that driver.
3. Return immediately from `dispatch` after routing an admitted command.
4. Yield live agent events from `next_event`.
5. Include the exact durable Ygg `EntryId` in every committed item.
6. Treat model/reasoning/resume changes as idle-boundary operations.
7. Keep private confirmation senders inside the driver and expose only opaque
   public request IDs.
8. Never infer tool activity, sources, changes, or artifacts from model prose.
9. Scope idempotency keys by authenticated device identity.
10. Retain free-form one-shot answers only as non-reversible digests plus their
    nonsecret command shape, preserving exact idempotency without retaining
    plaintext.

Run focused checks with:

```console
cargo test --manifest-path extensions/ygg-serve/Cargo.toml
```
