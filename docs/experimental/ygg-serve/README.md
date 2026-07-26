# Experimental `ygg serve`

`ygg serve` is an optional first-party extension and application surface for
using Ygg through a shared graphical client. It is not a new interaction mode,
and it does not replace or mirror the terminal UI.

The first release target is a polished local web client backed by real,
headless Ygg sessions. A later gate adds accountless, mutually authenticated
LAN clients. Thin macOS, iOS, and Android shells follow only after the web and
LAN contracts are stable.

## Product contract

- Opening the graphical app at its root creates and selects a fresh provisional
  session.
- Opening an explicit session route restores that session.
- Existing, pinned, and concurrently running sessions remain available in the
  sidebar.
- Each active session has one authoritative owner and runs independently.
- The transcript is the primary surface. Progress, sources, outputs, diffs,
  approvals, and previews appear only when real structured events create them.
- The interface has no Chat, Code, Work, Cowork, fleet, runtime, or dashboard
  mode.
- `ygg serve` is headless. It neither hosts nor synchronizes a TUI.

The graphical interaction grammar deliberately feels familiar to users of
ChatGPT and Claude/Cowork while remaining a clean-room Ygg implementation with
Ygg branding, terminology, themes, and security boundaries.

## Package boundary

Substantial implementation belongs outside Ygg's four core packages:

- `extensions/ygg-serve/` owns the protocol, session service, transports,
  security, and embedded-asset host.
- `apps/web/` owns the shared React client.
- Later thin native applications live under `apps/`.

`ygg-coding-agent` may contain only the smallest feature-gated adapter required
to construct and control its private `App` from the extension service, plus a
small `ygg serve` command dispatch. The default CLI, TUI, agent, AI, and
`sexy-tui-rs` behavior must not change when the feature is disabled.

The adapter and client are presentation-only boundaries. They must not add
presentation instructions to the model, alter the system prompt or active tool
schemas, insert frontend state into session content, or ask another model to
summarize work for the interface. Ygg's existing broad local authority remains
the default; client authentication and agent authority are separate controls.

See:

- [Architecture](architecture.md)
- [LAN pairing](lan-pairing.md)
- [Native delivery](native-delivery.md)
- [Web acceptance](web-acceptance.md)

## First web cut

The first complete vertical slice must use real sessions, not production
fixtures. It includes:

- a fresh-session launch flow and durable session sidebar;
- two independently running sessions;
- streaming assistant, reasoning, tool, approval, and run-outcome items;
- stop, steer, and queued follow-up;
- model, reasoning, authority, and supported attachment controls;
- deterministic sources, outputs, changes, and previews;
- replay, idempotency, reconnect, and replay-gap recovery;
- responsive desktop, tablet, and phone layouts;
- Ygg themes and the existing braille-tree icon.

Fixtures remain a development and test input only.

## Experimental build and test gates

The shipping binary keeps this surface disabled by default. Build or install
the experimental distribution explicitly with:

```console
cargo build --release -p ygg-coding-agent --features serve
cargo install --locked --path crates/ygg-coding-agent --bin ygg --features serve
```

Because `extensions/ygg-serve` is deliberately workspace-excluded, its focused
gate is mandatory in addition to the ordinary workspace gates:

```console
cargo test --manifest-path extensions/ygg-serve/Cargo.toml
cargo test -p ygg-coding-agent --features serve
```

`ygg serve` binds IPv4 loopback only in this cut. A one-use launch capability
is exchanged for an ephemeral, HttpOnly, same-site browser cookie before any
API or event-stream access. This transport authentication is distinct from
Ygg's agent authority and from the future LAN device identity described in the
pairing plan.

The public installer remains pinned to the current published release and must
not request the `serve` feature from a tag that predates it. Enabling `serve` in
that installer is a release gate: the installer pin, `--features serve`, signed
artifacts, and the installed-binary web-bundle smoke check must ship atomically
with a future `v0.3.2-alpha` or later tag.

## Explicit exclusions

The first web release does not include skills, MCP, plugins, extension
management, child agents, LSP, scheduling, an interactive terminal, TUI
synchronization, WAN access, multi-host replication, or a hosted account
service. Missing capabilities do not appear as empty navigation or dashboard
sections.
