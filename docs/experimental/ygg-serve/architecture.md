# Architecture

## Shape

```text
apps/web
  └─ transport-neutral client and deterministic reducer
       └─ ygg-serve protocol
            └─ host service
                 └─ session supervisor
                      └─ one session actor per graphical session
                           └─ feature-gated coding-agent adapter
                                └─ one private App/Agent owner
```

The frontend knows only the versioned protocol. It must not know whether its
transport is same-host HTTP/WebSocket, a future native bridge, or an
authenticated LAN connection.

## Extension packages

The first-party extension owns:

- bounded wire identifiers and payloads;
- host bootstrap and session catalog;
- authoritative session snapshots;
- typed item lifecycle events;
- command validation and idempotency;
- replay cursors and replay-gap recovery;
- session actor and supervisor orchestration;
- loopback HTTP/WebSocket transport;
- preview and artifact capability handles;
- production web assets;
- future device identities and LAN transport.

It must not expose `App`, provider credentials, unrestricted paths, raw process
handles, or internal TUI state over the wire.

## Coding-agent adapter

`App` is private to the binary package. A standalone extension crate cannot
truthfully create real Ygg sessions without an adapter at that ownership
boundary.

The allowed adapter:

- is feature-gated;
- lives under the coding-agent extension integration area;
- constructs a new `App` for a requested session;
- translates agent/session lifecycle into renderer-neutral extension events;
- accepts only validated typed commands;
- preserves exactly one mutable owner per session;
- performs no presentation work;
- depends on the extension's host-facing trait rather than making the extension
  depend on coding-agent internals.

The feature-enabled package runtime keeps the internal dispatch into this
adapter tiny. The ordinary Ygg binary instead keeps a tiny external `ygg serve`
dispatch into the installed runtime. Broader changes to the AI, agent, TUI, or
terminal packages are out of scope.

## Session ownership

One supervisor owns the graphical session catalog. Every active graphical
session has one actor and one `App`/Agent owner. Multiple clients may observe or
control that actor, but they may not create competing owners.

Each client keeps private presentation state such as:

- selected session;
- scroll position;
- open inspector and pane size;
- unsent composer draft.

Commands that affect shared execution carry stable command IDs. Repeating the
same command returns the original acknowledgement and never executes twice.

## Bootstrap modes

`GET /api/v1/bootstrap` creates and selects a provisional session by default.
`selectedSessionId` restores an explicit session. `inventoryOnly=true` returns
catalog state without creating, opening, or selecting a session; its
`selectedSessionId` and `selectedSession` fields are both `null`. The inventory
and explicit-selection query modes are mutually exclusive. Bootstrap catalog
cursors are anchored before asynchronous project/session listing, so a catalog
change racing the snapshot always retains a newer replayable revision instead of
being hidden behind stale list data.

## Item lifecycle

The protocol distinguishes provisional streaming state from durable committed
entries:

1. an item is created with a stable item ID;
2. bounded deltas update that item;
3. completion replaces provisional fields with authoritative content;
4. durable commit attaches the exact session entry identity;
5. reconnect either replays missing ordered events or replaces the state with a
   complete authoritative snapshot.

The client reducer must be deterministic and idempotent. Tool state, sources,
outputs, and changed files are derived from typed evidence, never from assistant
prose or command-shaped text.

## Pull-request projection

The coding-agent adapter discovers a session-associated pull request only from
structured `gh pr view --json number,url,state,isDraft` output. First-time
association remains disabled until the session admits user work; a bounded,
independent host refresher then keeps both hosted and inventory-only sessions
current without delaying agent commands. CLI execution is non-interactive and
shell-free, with null stdin, bounded output and concurrency, a four-second
timeout, and strict HTTPS pull-request URL validation. Hosted refreshers wait
fairly for detached query permits, while inventory work uses only immediately
available capacity so a large inactive catalog cannot starve live sessions.
Inventory evidence is attempted oldest-first in bounded rounds, so a temporarily
unavailable record cannot indefinitely pin every later session behind it.

The private `pull-requests-v1.json` sidecar stores the session ID, validated URL,
number, state, and refresh time. It is size/count bounded, opened without following
symlinks, and replaced atomically. Invalid or ambiguous persisted evidence fails
host startup. Evidence persistence and inactive transcript projection run on the
blocking pool rather than the agent runtime path; command-side projection
replacements read the actor's in-memory PR summary instead of contending with
that persistence lock. Temporary GitHub or CLI failure preserves the last valid
state; authoritative closure removes open evidence, while merged evidence is
terminal. Permanent deletion fences the session before removing evidence, so an
already-finishing discovery cannot recreate the sidecar record.
`session.pullRequestChanged` advances the actor's catalog projection and replay
sequence, never the durable conversation transcript. Bootstrap overlays retain
the host catalog's durable PR projection when an active actor view is waiting on
a backpressured evidence event. The web store replaces sidebar/command-center PR
evidence only from the catalog stream, so a delayed session envelope cannot
regress a newer hosted or inventory projection.

## Local transport

The first web gate binds only to loopback and retains strict host/origin
validation, request/frame limits, security headers, and sanitized errors. It
must not gain LAN access by binding the same unauthenticated server to
`0.0.0.0`.

## Local terminal

When sandbox policy permits process execution, `ygg-serve` owns a bounded
in-process PTY manager and exposes it only through the authenticated,
same-origin loopback WebSocket. A browser owner key reattaches to a retained
shell after disconnect, while the manager limits retained sessions to four and
bounds input, replay, and dimensions. A terminal is rooted at the configured
workspace; it is not a general path or remote-shell API. Server shutdown stops
all retained shells.

## Preview isolation

Generated HTML and live previews use a separate, capability-limited surface.
They cannot access the main application DOM, provider credentials, arbitrary
host files, process APIs, or unrestricted navigation. Preview closing changes
presentation only; it does not stop a session or development server.
