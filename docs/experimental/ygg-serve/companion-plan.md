---
stage: engineering-design
status: ready-for-implementation
updated: 2026-08-14
owner: ygg-serve
---

# Ygg Serve worldwide companion plan and engineering design

## Product Brief

### Problem

A Ygg user can supervise several agents and projects from the responsive Serve web client only while they can reach the authoritative machine's loopback interface. Leaving that machine means losing visibility and control even though the agents continue to run there. Exposing the existing loopback listener would discard its authentication and origin assumptions and would put provider credentials, host paths, and broad local agent authority behind an unsuitable WAN boundary.

### Target user

The first user is one technical Ygg owner who runs one always-on or frequently available authoritative host and wants to check, steer, stop, and continue that host's existing agents from their own iOS or Android device while away from the host network. This release is not for teams, shared hosts, or users who need agents to run on the phone.

### Evidence and assumptions

Observed repository evidence:

- `ygg serve` already owns multiple independent projects and session actors behind one `SessionSupervisor`.
- `apps/web` is responsive at phone widths and already owns deterministic reconnect, replay, snapshot replacement, command idempotency, uploads, downloads, resources, and event reduction.
- The production transport is intentionally loopback-only and relies on one-use launch authentication, strict Host/Origin checks, bounded requests, and bounded WebSockets.
- Connected Devices is currently fixture-only, direct browser networking is spread across fetch, WebSocket, download, resource, preview, image, and terminal call sites, and command envelopes currently trust caller-provided `deviceId`.
- A local Tauri 2 spike compiles, supports a narrow native-owned boundary, and can use OS-protected key storage on macOS, iOS, and Android.
- Iroh 0.95.1 supports Rust 1.86, authenticated endpoint identities, explicit relay maps, bounded QUIC streams, stream cancellation, and direct/relay connectivity.

Explicit assumptions to validate during the experimental rollout:

- One owner-controlled host is online often enough that an online-only companion is valuable.
- n0 relay availability and metadata exposure are acceptable to this early cohort when explicitly enabled and documented.
- A Tauri-owned loopback proxy is reliable in foreground mobile use and avoids platform WebView custom-protocol differences.
- Foreground reconnect is sufficient for v1; push notifications and background execution are not required for the value moment.

### Chosen direction

Ship one opt-in worldwide companion mode for one authoritative host. The host and each device have persistent Iroh endpoint identities. The owner opens a short-lived pairing invitation in the authenticated loopback UI, confirms and approves the pending device, and can revoke it later. Operational traffic uses an application-owned, versioned protocol over Iroh QUIC. Worldwide reachability uses only an explicitly selected n0 relay map; Ygg does not enable Iroh's N0 preset, DNS discovery, or pkarr publication/resolution.

The mobile app is a thin Tauri 2 shell. Native Rust owns endpoint keys, host pins, remote connections, and the local proxy. The exact built `apps/web` assets talk only to the app-owned loopback origin. The native proxy translates bounded local HTTP/WebSocket operations into the companion QUIC protocol, so browser content never receives host credentials or dials the host. Terminal/TUI streaming remains unavailable to paired devices.

## Product Plan

### Outcome and Product Principles

A paired owner can open the native Ygg companion from another network, see the authoritative host's real projects and agents, issue the same non-terminal controls as the web client, survive an ordinary disconnect through replay, and revoke the device from the local owner interface.

Principles:

1. The host stays authoritative; mobile is a view/controller, never an agent runtime or replica.
2. Secure explicit opt-in beats automatic reachability. Existing loopback defaults and owner behavior do not change.
3. Authentication grants transport access, not additional agent/tool authority.
4. Reuse the existing web product and replay semantics rather than creating a second mobile product or protocol reducer.
5. Fail closed on host identity, device identity, protocol, bounds, revocation, and unsupported terminal access.
6. Keep secrets in native/host security boundaries and keep n0's metadata visibility explicit.

If nothing is built, users must remain near the host or build an unaudited tunnel. Existing responsive UI and session supervision solve the interaction and authoritative-state portions, but not WAN identity, reachability, native secret ownership, or owner-approved pairing.

The smallest complete outcome includes pairing, one real operational request, live events, command control, reconnect/replay, and revocation. A bootstrap-only demo would not be complete. The substantially better experience at acceptable incremental scope is a generic bounded proxy for all existing non-terminal operational routes, because it preserves uploads, downloads, images, exports, and previews without a growing set of mobile-only gaps.

### Primary User Journey

1. The owner starts `ygg serve` with companion mode and n0 relay selection explicitly enabled. The normal loopback app still opens as before.
2. In **Connected devices**, the owner selects **Pair a device**. Ygg creates one short-lived, one-use invitation and shows a copyable/scan-ready payload. No verification phrase exists yet.
3. In the native app, the user imports the invitation and names the device. The app pins the advertised host endpoint identity and submits its own authenticated endpoint identity and fresh request nonce.
4. After that authenticated request arrives, both surfaces show the same request-bound verification phrase. The local owner explicitly approves or denies the pending request.
5. On approval, the app stores its endpoint key with OS-protected storage, acknowledges durable storage, and opens the shared Ygg interface.
6. The user views all projects/tasks on that host, opens a task, receives live events, and sends a command. The value moment is seeing and controlling the same authoritative running agent from a different network.
7. A transient disconnect leaves visible state and drafts in place. Reconnect resumes events and uses existing replay/snapshot replacement.
8. The owner revokes the device locally. Active remote work closes and reconnect is denied without changing agent authority or host state.

### Scope: Now

- One authoritative host containing any number of existing Serve projects and sessions.
- Explicit `--companion` enablement and explicit `--companion-relay n0` WAN selection.
- Persistent host and device endpoint identities.
- Short-lived, one-use invitation; pinned host identity; matching verification phrase; explicit local-owner approval; poll; durable-storage acknowledgement.
- Authoritative owner device list, pending-request decisions, and revocation.
- Tauri 2 mobile shell with native-owned keyring credentials and a native-owned loopback proxy.
- The exact synchronized production web bundle after connection.
- Every current non-terminal operational HTTP route, binary body, upload, download, resource, export, and event stream through the bounded native/QUIC boundary.
- Principal-to-command-envelope `deviceId` equality before supervisor dispatch.
- Foreground reconnect plus existing replay and replacement-snapshot behavior.
- Clear offline, pending, denied, expired, identity-changed, revoked, protocol-mismatch, and relay-unavailable states.
- Experimental macOS compilation as a development proxy for shared Tauri Rust code; iOS/Android projects and physical builds remain gated by local platform tooling.

### Requirements

1. **Explicit startup:** When an owner runs `ygg serve` without companion flags, Ygg binds and behaves exactly as the current loopback-only product and performs no Iroh, relay, DNS, pkarr, or companion-state work.
2. **Explicit WAN provider:** When the owner enables companion mode, WAN relay use starts only with an explicit supported relay selection. Selecting n0 constructs a reviewed custom n0 relay map from `Endpoint::empty_builder`; it never invokes `Endpoint::builder`, `Endpoint::bind`, `RelayMode::Default`, or the N0 preset.
3. **One-host native client:** A mobile installation pairs with and controls one host at a time. Replacing that host requires an explicit remove/re-pair action.
4. **Pinned host trust:** The native client accepts operational traffic only from the exact endpoint identity in the approved pairing invitation. Identity or protocol changes block access and offer re-pairing; there is no continue-anyway action.
5. **Owner-approved pairing:** A valid invitation creates only a pending request. No device becomes operational until the authenticated loopback owner approves and the native client acknowledges protected storage.
6. **Native secret ownership:** Host and device endpoint private keys never enter JavaScript, URLs, logs, crash text, public DTOs, generic serialized application state, or broad `Debug`/`Display` output. The mobile key uses OS-protected storage and iOS ThisDeviceOnly accessibility.
7. **Principal binding:** For every remote host/session command, the authenticated endpoint principal and command-envelope `deviceId` must match before `SessionSupervisor` sees the command. Mismatch is a non-retryable forbidden response with no mutation or idempotency-cache entry under the claimed identity.
8. **No authority expansion:** Pairing exposes only existing typed Serve operations and cannot reveal provider credentials, accept unrestricted host paths, alter the authority ceiling, enable Remote Read, or grant device administration to a paired device.
9. **No remote terminal:** Bootstrap for a paired principal advertises terminal unavailable, and both host and native boundaries reject terminal/TUI routes even when local process execution is enabled.
10. **Complete audited traffic boundary:** Web content communicates only with its app-owned loopback origin. API requests, events, uploads, downloads, attachments, exports, previews, images, opaque resources, cancellation, reconnect, and replay cross the native/QUIC boundary; the web content does not hold remote endpoints or credentials.
11. **Bounded protocol:** Every header, path, query, body chunk, aggregate request/response, event, pairing payload, connection, stream, queue, timeout, and retry has a named limit. Oversize or malformed input is rejected before application work.
12. **Backpressure and cancellation:** QUIC and bounded queues apply producer backpressure. Closing/aborting the local operation resets or stops the corresponding QUIC stream, and lagged event consumers reconnect through existing replay rather than accumulating unbounded state.
13. **Replay convergence:** After reconnect, the shared TypeScript client retains its visible snapshot and drafts, requests replay for known cursors, buffers live events, and replaces from an authoritative snapshot on gaps or generation changes without duplicate execution.
14. **Immediate revocation:** Only the loopback owner can revoke. Revocation is durably committed before active remote connections are closed; later streams and reconnects fail as revoked.
15. **Authoritative device UI:** Connected Devices is shown only when companion mode is healthy. It uses host data for invitations, pending decisions, paired/revoked devices, and connection state; fixture-local mutation is never production behavior.
16. **Visual/product reuse:** Once paired, the mobile app renders the synchronized production `apps/web` output and keeps the established responsive Ygg layout, terminology, themes, accessibility, and reduced-motion behavior.
17. **Sanitized failures:** The user receives a bounded actionable state for validation, permission, identity, protocol, expiry, denial, revocation, timeout, relay, host-offline, and internal failures. Secrets, remote transport internals, provider data, and host paths are never exposed.
18. **Independent packaging and MSRV:** The optional backend remains independently buildable, substantive networking remains outside core agent/AI/TUI crates, and host/native Rust dependencies compile with Rust 1.86.
19. **Relay privacy disclosure:** Before enabling n0 relay mode, documentation states that payloads remain end-to-end encrypted while n0 can observe connection metadata such as endpoint timing, relay region, and traffic volume.
20. **Safe state ownership:** Host companion state remains under the configured protected Serve state root using no-follow, regular-file, private-mode, bounded, synced, atomic persistence; setup never tightens permissions on broad parent directories.

### Experience States

- **First use:** Native onboarding explains that a running host and owner-generated invitation are required; it offers paste/import now, not an account login.
- **Empty owner list:** Connected Devices explains that no device is paired and offers a pairing invitation only when companion mode is healthy.
- **Loading/connecting:** Keep the shell deterministic, show the named host and a cancellable connection state, and do not imply agent progress.
- **Pending:** Both devices show device name, verification phrase, expiry countdown, and cancel/deny actions. Mobile cannot enter the operational UI.
- **Success:** Mobile transitions to the exact shared interface and identifies the pinned host. Owner UI shows the paired device.
- **Validation failure:** Invalid, malformed, oversized, reused, or expired invitations remain non-destructive and request a new invitation.
- **Permission/storage failure:** Pairing does not complete if secure storage is unavailable or locked. The UI asks the user to unlock/enable storage and retry.
- **Recoverable network failure:** Preserve visible host state and drafts, show a non-modal disconnected indicator, and retry with the existing bounded foreground schedule.
- **Identity/protocol failure:** Block, name the expected host, and require update or explicit re-pairing. Never auto-accept a replacement key.
- **Revoked:** Close the operational connection, preserve an unsent local draft for user recovery where feasible, remove stored host access, and return to pairing.
- **Cancellation:** Cancelling pairing or a transfer ends its stream and leaves no approved device or partial application mutation beyond idempotent operations already acknowledged.

### Trust and Accessibility Constraints

- The loopback cookie principal alone administers pairing and devices.
- Paired devices receive read/control only; destructive session commands still use existing confirmation contracts.
- Device names, platform strings, and versions are untrusted presentation text and are bounded/sanitized.
- Invitation, poll, and acknowledgement capabilities are independent, short-lived, memory-only values.
- The relay provider cannot decrypt application bytes but is not hidden from the user.
- Pairing does not weaken project trust, authority profiles, approvals, path confinement, resource opacity, CSP, same-origin rules, or external-request audits.
- Pairing and reconnect states are keyboard accessible, do not rely on color alone, preserve visible focus, and honor reduced motion.

### Success Metrics and Instrumentation

Primary metric: during a four-week opt-in dogfood, at least 80% of approved pairings complete one real cross-network bootstrap, event receipt, and acknowledged non-terminal command within ten minutes.

Guardrails:

- 0 credential, endpoint-private-key, provider-secret, or unrestricted-path leaks in automated scans and reviewed logs.
- 0 command mutations from a mismatched or revoked principal in adversarial tests.
- At least 99% replay convergence in 100-cycle deterministic disconnect tests with no missing/duplicate committed item.
- Existing loopback web acceptance remains green.
- Median foreground reconnect under healthy relay conditions is below five seconds; no unbounded memory growth in bounded-transfer tests.

Local-only instrumentation events/counters (no outbound product telemetry): companion start result, relay choice, pairing opened/requested/approved/denied/acknowledged/expired, connection direct/relay/failed, request route class/status/bytes/duration, event lag/reconnect/replay-gap, cancellation, and revocation. IDs are hashed or omitted; secrets, paths, prompts, command bodies, and provider data are excluded.

Kill/reconsider threshold: disable experimental distribution if any key/pin leak, unauthorized mutation, silent identity replacement, or repeatable replay corruption is found; reconsider n0 as the default supported option if its measured cross-network completion is below 70% or metadata terms become incompatible.

### Launch and Rollback

- Cohort: maintainers and explicitly invited technical dogfood users with one owner-controlled host.
- Onboarding: experimental documentation, relay privacy disclosure, exact CLI opt-in, pairing/revocation walkthrough, foreground-only expectations, and platform prerequisites.
- Rollout order: protocol/host harness, desktop Tauri check, iOS simulator/device, Android emulator/device, then limited cross-network dogfood.
- Feedback: local diagnostics export with secrets removed plus an issue template for host/mobile versions, relay/direct status, and failure code.
- User rollback: stop passing companion flags; loopback remains available and paired registry data stays dormant. Revoke all devices or remove companion state only through an explicit documented reset flow.
- Release rollback: remove/disable the native build and companion flags without migrating or rewriting session/project state.

### Dependencies and Risks

- n0 relay availability, policy, regional performance, and observable metadata.
- Iroh 0.95.1 API stability while Ygg remains on Rust 1.86.
- Mobile foreground/network lifecycle differences and local-proxy behavior.
- Apple signing, CocoaPods, simulator runtimes, Android SDK/Gradle, and physical devices are external release blockers, not reasons to weaken the contract.
- Tauri keyring backend behavior must be verified on physical iOS/Android devices.
- Generic route proxying lowers frontend churn but requires a strict shared allowlist so future host routes are not exposed automatically.

### Later

- Multiple saved hosts with explicit switching (not aggregation).
- User-selected self-hosted relay maps.
- Background notifications and deep links.
- Native photo/file pickers and richer platform integration.
- QR camera scanning after paste/import proves the ceremony.
- Direct-connect diagnostics and relay-region selection.
- A deliberate LAN migration/reconciliation plan if the older TLS/mDNS LAN design is retained.

### Non-goals

- On-phone agent, model, provider credential, tool, shell, or durable session runtime.
- Multi-host aggregation, automatic failover, peer replication, conflict resolution, or offline execution/transcript cache.
- Hosted Ygg accounts, cloud control plane, OAuth, analytics, or cloud rendezvous operated by Ygg.
- Browser-based remote access, direct exposure of the loopback listener, CORS access, or credentials in web storage.
- Terminal, PTY, TUI, arbitrary process, unrestricted filesystem, provider, or extension-management access from a paired device.
- Automatic UPnP/port forwarding, implicit relay enablement, N0 presets, DNS/pkarr publication, or hidden relay selection.
- Team sharing, roles, device-to-device administration, or one paired device approving another.
- Replacing the existing agent authority, project trust, approvals, idempotency, replay, or local owner contracts.

### Open Decisions

None blocking. Relay vendor expansion, multi-host switching, background operation, and the relationship to the older LAN specification are explicitly later decisions.

### Product Decision Log

- 2026-08-14: Chose one-host worldwide companion instead of multi-host aggregation or mobile execution.
- 2026-08-14: Chose Tauri 2 and the shared responsive web bundle; rejected Electron and platform-specific product forks.
- 2026-08-14: Chose Iroh 0.95.1 authenticated QUIC with explicit n0 relay map; prohibited N0 presets and DNS/pkarr.
- 2026-08-14: Chose owner-approved pinned pairing and endpoint-key device credentials.
- 2026-08-14: Chose a native-owned loopback proxy so every existing browser networking form crosses one audited native boundary.
- 2026-08-14: Kept terminal, remote browsers, background execution, and multi-host behavior out of v1.

## Engineering Design

### Goals, Non-goals, Constraints, and Invariants

The engineering goal is the smallest secure end-to-end slice that starts only under explicit CLI flags, pairs one native endpoint through local-owner approval, proxies all current non-terminal Serve operations, preserves replay/idempotency, and revokes immediately.

Invariants:

- `HostService` and `SessionSupervisor` remain the shared application boundary. Transport authentication never enters `ygg-agent` or changes model/tool behavior.
- Loopback authentication, bind address, launch URL, cookie, Host/Origin/Fetch Metadata checks, CSP, and defaults remain unchanged.
- A remote endpoint identity maps to exactly one durable `DeviceId`; every remote command uses that mapping before supervisor dispatch.
- Host endpoint key, device endpoint key, invitation, and poll token are distinct. Host/device endpoint keys survive restart; invitation/poll capabilities do not.
- The host endpoint ID pinned at pairing is the authority. Endpoint addresses and relay URLs are mutable location hints within the signed/authenticated Iroh connection.
- Operational remote routes are an exact allowlist. Pairing administration and terminal are never included.
- Every byte has a cumulative and per-frame bound. Backpressure is inherited from awaited QUIC writes and bounded local queues; no unbounded fan-out is introduced.
- Existing event ordering, actor generations, cursor replay, replacement snapshots, and command IDs remain authoritative.

Non-goals are the product non-goals above. SQL/data migrations, cloud services, and cross-host consistency do not apply because state remains one protected local registry next to existing Serve state.

### Current System and Reuse Map

| Existing component | Reuse | Change |
|---|---|---|
| `extensions/ygg-serve/src/service.rs::HostService` | Unchanged application adapter boundary | None |
| `SessionSupervisor` | Bootstrap, catalog, typed commands, resources, replay, `subscribe_events()` | Receives remote commands only after principal binding |
| `transport.rs` handlers/router | Existing HTTP semantics, limits, errors, uploads/downloads | Split route construction from loopback authentication; add owner-only companion administration; add principal-sensitive capabilities |
| `TransportAuth` / `secure_request` | Existing loopback cookie and request checks | Keep loopback-only; attach an internal loopback-owner principal after authentication |
| `events_socket` / web reconnect | Existing bounded live delivery and replay-first convergence | Companion event stream adapts the same broadcast receiver to event records |
| `apps/web` `HttpTransport` and direct same-origin URLs | Existing API encoding/decoding, binary URLs, WebSocket reconnect | Continue targeting one same-origin local proxy in Tauri; add real owner device-management methods/UI |
| `embedded_web::WebBundle` and synchronized bundle | Hash-validated production assets | Tauri embeds the same allowlisted files/manifests and serves them from its loopback origin |
| secure Serve state helpers/patterns | No-follow/private/atomic persistence model | New companion subdirectory and narrow endpoint-secret wrapper |
| command envelope `DeviceId` | Existing idempotency scope | Companion boundary rejects principal mismatch before dispatch |
| `apps/web` terminal path | Local terminal unchanged | Paired bootstrap advertises false; host and native allowlists reject route |

The older `lan-pairing.md` remains a separate unimplemented LAN contract. This WAN design reuses its authoritative-host, owner-approval, revocation, and secret-handling principles, but does not silently redefine its TLS/mDNS/no-WAN scope.

### Architecture and Data Flow

```text
Local owner browser
  -> existing 127.0.0.1 HTTP/WS + launch cookie
  -> owner-only pairing/device routes -----------+
                                                  |
Tauri WebView (exact apps/web bundle)             |
  -> app-owned 127.0.0.1 HTTP/WS proxy            |
     (one-use local cookie, exact routes)          |
  -> native Rust companion client                 |
     (OS keyring: device endpoint key)             |
  -> Iroh QUIC, ALPN ygg/companion/1               |
     direct path or explicit n0 relay              |
  -> companion listener                           |
     remote_id -> durable DeviceId                 |
     exact route + bounds + principal binding      |
  +----------------------+-------------------------+
                         v
              shared Axum application routes
                         v
             SessionSupervisor / actors
                         v
          authoritative coding-agent App(s)
```

New/proposed ownership:

- `extensions/ygg-companion-protocol/`: small Rust-1.86 shared framing, route allowlist, pairing ticket, bounded public DTOs, and fingerprint function. It contains no Iroh or Ygg-agent dependency.
- `extensions/ygg-serve/src/companion.rs`: host endpoint, secret persistence, registry/pairing state, authenticated QUIC accept loop, owner control facade, event adaptation, and principal binding.
- `extensions/ygg-serve/src/transport.rs`: shared application router and existing loopback router, internal principal, principal-sensitive bootstrap, owner-only administration routes.
- `apps/mobile/src-tauri/`: Tauri shell, keyring-backed device key, host profile, Iroh client, local authenticated proxy, onboarding, and exact bundled web assets.
- `apps/web`: real device catalog/pairing UI and transport methods only. Session/event reducers remain unchanged.

### Components and Contracts

#### CLI and configuration

Proposed CLI:

```text
ygg serve [existing options]
  --companion
  --companion-relay n0
```

`--companion-relay` requires `--companion`. Companion without a relay is reserved for direct-address development only if explicitly implemented; the worldwide documented command includes both flags. Unknown relay values fail CLI parsing. Default values are `false`/`None`, so no endpoint or state opens by default. The external extension dispatcher forwards flags exactly.

`CompanionConfig` contains only the protected Serve state root, public host descriptor, and explicit relay enum. It must not implement a secret-bearing debug representation.

#### Endpoint construction

Both host and mobile call:

```rust
Endpoint::empty_builder(RelayMode::Custom(explicit_n0_relay_map()))
    .secret_key(persisted_key)
    .alpns(vec![COMPANION_ALPN.to_vec()])
    .bind()
    .await
```

The reviewed map explicitly enumerates the four production n0 relay configurations. No discovery service is added. Calls to `Endpoint::builder()`, `Endpoint::bind()`, `RelayMode::Default`, `presets::N0`, `PkarrPublisher`, and n0 DNS resolution are forbidden by source audit.

ALPN is `ygg/companion/1`. The major version is also present in every request head; either mismatch is terminal and never retried automatically.

#### Pairing ticket

The application-owned ticket is bounded to 4096 encoded bytes:

```text
ygg://pair/v1/<base64url(canonical-json)>
```

Canonical payload:

```json
{
  "protocol": 1,
  "hostId": "host-...",
  "hostEndpointId": "iroh-public-key",
  "relayUrls": ["https://..."],
  "directAddresses": ["ip:port"],
  "invitation": "base64url-256-bit-secret",
  "expiresAtMs": 0
}
```

There is one active invitation, 120-second TTL, one valid request, and at most three pending requests. The invitation is memory-only and is not logged. Address arrays are bounded and treated only as hints. The client builds `EndpointAddr::from_parts()` from the pinned endpoint ID and validated hints.

Pairing operations are typed control requests over the same ALPN:

- `request`: protocol, invitation, random request nonce, observed HostId/endpoint ID, bounded device name/platform/app version. The authenticated QUIC `remote_id()` is the claimed device endpoint; no body may override it.
- `status`: request ID plus independent 256-bit poll token.
- `ack`: request ID plus poll token after keyring/profile persistence succeeds.
- `cancel`: request ID plus poll token.

The verification phrase is created only after the host authenticates the request's QUIC endpoint. Its derivation binds the protocol label, HostId, pinned host endpoint ID, authenticated device endpoint ID, client request nonce, and invitation secret. The invitation response therefore contains only the ticket and expiry; it cannot display a request-verification phrase before a device request exists.

The initial request response contains request ID, poll token, phrase, state, and expiry. Approval returns assigned `DeviceId` and public host profile, not another bearer credential. The persistent device endpoint private key is the credential. Ack durably activates the endpoint-to-device mapping and consumes/zeroizes pairing capabilities.

#### Owner administration

Mounted only behind existing loopback authentication:

```text
GET    /api/v1/companion/devices
POST   /api/v1/companion/pairing/open
GET    /api/v1/companion/pairing/state
POST   /api/v1/companion/pairing/requests/{requestId}/decision
DELETE /api/v1/companion/pairing
DELETE /api/v1/companion/devices/{deviceId}
```

Decision bodies accept exactly `{"decision":"approve"}` or `{"decision":"deny"}`. Open is idempotent while valid. Close, denial, expiry, acknowledgement, and revocation are terminal/idempotent. The companion operational allowlist excludes the entire `/api/v1/companion/` prefix.

#### QUIC request framing

One logical operation uses one bidirectional QUIC stream. Integers are unsigned big-endian. JSON uses UTF-8, camelCase, and unknown-field denial.

```text
u32 head_length (1..=16 KiB)
head JSON
operation-specific records
```

Request head:

```json
{
  "protocol": 1,
  "requestId": "bounded random id",
  "kind": "http | events | pairing",
  "method": "GET | POST | DELETE",
  "path": "/allowlisted/path?bounded=query",
  "contentType": "optional allowlisted media type"
}
```

HTTP request body records:

```text
u32 chunk_length (0 means end; otherwise <= 64 KiB)
chunk bytes
```

Aggregate request limits are route-specific: commands/search/goals 512 KiB, attachments 5 MiB, documents 8 MiB, project writes use the existing encoded-write bound, and no-body routes require immediate end. Query is at most 4 KiB and export/resource shape restrictions match loopback.

Response head is at most 16 KiB and contains protocol, matching request ID, status, and only audited response headers: content-type, content-disposition, content-length, cache-control, etag, x-content-type-options, referrer-policy, and cross-origin-resource-policy. HTTP body records use the same 64 KiB chunk framing. Aggregate response limits are route-specific: event 1 MiB each, snapshot 8 MiB, bootstrap 12 MiB, attachment 5 MiB, resource 8 MiB, export 64 MiB, and a conservative 12 MiB for other operational JSON.

An event subscription receives one response head and then records:

```text
u32 event_length (1..=1 MiB)
event JSON
```

Each event passes existing `HostStreamEvent::validate()` before encoding. Broadcast lag closes/resets the stream with `replayRequired`; TypeScript reconnect/replay owns recovery.

Application reset codes:

| Code | Meaning |
|---|---|
| `0x10` | client cancellation |
| `0x11` | malformed/oversized frame |
| `0x12` | protocol mismatch |
| `0x13` | unknown/unauthorized endpoint |
| `0x14` | revoked endpoint |
| `0x15` | event lag/backpressure; replay required |
| `0x16` | sanitized internal failure |

Closing the local HTTP body/WebSocket or aborting a fetch resets the request send side and stops the response receive side. Host response cancellation drops the Axum body/handler future. Exact command retries remain TypeScript-owned and retain the same serialized body/command ID.

#### Route allowlist

The shared protocol crate classifies exact current non-terminal operational routes and returns request/response limits. It rejects absolute/scheme/authority URLs, fragments, backslashes, encoded separators, dot segments, unknown methods, arbitrary headers, static assets, launch routes, pairing administration, and terminal. Future host routes remain remote-inaccessible until deliberately added and tested in this classifier.

#### Principal binding

The Iroh connection's `remote_id()` is resolved against a non-revoked registry entry for every stream. The companion server inserts an internal `TransportPrincipal::Paired { device_id }` before calling shared application routes.

For `/api/v1/commands/host` and `/api/v1/commands/session`, the companion boundary buffers only the already bounded command body, strictly decodes the appropriate envelope, and compares its `deviceId` to the principal. A mismatch returns 403 before `SessionSupervisor::host_command` or actor dispatch. The boundary does not rewrite arbitrary command content.

Loopback continues to accept its existing browser device IDs. Principal-sensitive bootstrap changes only transport capabilities: paired principals get `terminal=false` and no device-administration capability; loopback owner gets Connected Devices only when the companion runtime is healthy.

#### Native loopback proxy

The proxy binds IPv4 loopback on port zero, generates one process-local launch token/cookie, validates exact Host/Origin/Fetch Metadata, and applies the existing CSP/security-header policy. It serves only the seven synchronized production assets plus its native onboarding assets. The web bundle receives no endpoint, key, relay URL, poll token, or invitation after pairing.

Before loading the production index, native injects only the public assigned `DeviceId` as the existing `ygg-device-id` metadata. This makes shared command encoding match the host principal without exposing a credential. The source bundle and JS/CSS bytes remain exact and hash-validated.

The proxy maps allowed local HTTP operations to one QUIC stream and `/api/v1/events` WebSocket to one event stream. `/api/v1/terminal`, unknown paths, CORS, child-frame privileged navigation, and arbitrary outbound navigation are rejected. Web content sees only same-origin HTTP/WS and therefore existing direct links, object URLs, images, exports, cancellation, and replay all cross the native boundary without a second TypeScript protocol implementation.

#### Native storage

- Keyring account `companion-endpoint-key-v1`: exactly 32 raw secret bytes via `set_bytes`/`get_bytes`.
- iOS write accessibility: `WhenUnlockedThisDeviceOnly` (or stricter passcode-set policy after physical-device validation).
- Non-secret host profile: version, HostId, pinned endpoint ID, bounded address hints, assigned DeviceId, and timestamps in app-private storage with atomic replacement.
- The endpoint-key wrapper exposes only `public_id()`, `load()`, `create()`, and `store()`; its `Debug` is redacted and it has no `Display`, serde, or public byte getter.
- If profile and key disagree, keyring is unavailable, or the stored profile is malformed/oversized, startup fails closed into recovery/removal UI.

### Data, State, and Concurrency

Host layout:

```text
<session-root>/.serve/companion-v1/       mode 0700
  endpoint-key-v1                         mode 0600, raw 32 bytes
  devices-v1.json                         mode 0600
```

Registry:

```json
{
  "version": 1,
  "revision": 12,
  "hostId": "host-...",
  "hostEndpointId": "public-key",
  "devices": [
    {
      "id": "device-...",
      "endpointId": "public-key",
      "name": "Phone",
      "platform": "ios",
      "pairedAtMs": 0,
      "lastSeenAtMs": 0,
      "revokedAtMs": null
    }
  ]
}
```

The registry is limited to 128 retained devices and 256 KiB. IDs/endpoint IDs are unique; timestamps are monotonic enough for display but not authorization. Updates use exclusive no-follow temp creation, file sync, atomic rename, and directory sync. Once either identity file is committed, a missing/malformed endpoint key or registry, an endpoint mismatch, or a changed stable host ID fails startup; identity is never regenerated silently. `lastSeenAtMs` persistence is throttled.

In-memory pairing state has one invitation, at most three pending requests, and approved data only until ack/expiry. One mutex serializes transitions; exact request nonce retries return the original state. Secret values zeroize on terminal transitions/drop.

The endpoint accept loop has bounded global connection and per-device stream semaphores. Every device connection subscribes to a revocation broadcast. Revocation first commits the registry revision, then broadcasts closure; stream admission rechecks the registry so reconnect cannot race the commit. Event receivers use the existing bounded broadcast channel. QUIC flow control and awaited writes provide transport backpressure.

### End-to-End Flows

#### Startup

1. Parse CLI relationships. No flags follows the unchanged loopback path.
2. With companion enabled, validate/create the private companion directory, load/create endpoint key, validate registry, build explicit relay map, and bind Iroh.
3. Build shared application router once. Clone it beneath existing loopback authentication and pass a clone to the companion accept loop.
4. Start loopback listener. If explicit companion startup fails, fail the command with an actionable error rather than silently running loopback-only.
5. Shutdown closes local HTTP, Iroh endpoint/connections, event tasks, and PTYs within existing lifecycle behavior.

#### Pairing

1. Owner opens invitation; host snapshots current endpoint hints and creates secret/expiry.
2. Native decodes bounded ticket, checks expiry/protocol, creates/loads device key, pins endpoint ID, and connects.
3. Host verifies invitation, observed identities, and remote endpoint; consumes ticket into Pending and returns poll capability/phrase.
4. Owner state polling shows pending request; owner approves.
5. Native status poll receives approval, atomically stores profile after key already exists, then sends ack.
6. Host commits active registry entry and consumes pairing state. Native opens operational proxy.
7. Any storage failure prevents ack; expiry removes approval and no active mapping exists.

#### Operational request

1. Web emits existing same-origin request.
2. Native validates cookie, method/path/content type/body limit, and opens QUIC stream.
3. Host authenticates `remote_id`, validates framing/route/limits, and checks command identity when applicable.
4. Host invokes cloned application router. Existing handler/supervisor semantics run once.
5. Response headers/body stream back through bounded records and native returns same-origin response.
6. Browser cancellation resets/stops QUIC; a fully acknowledged command remains governed by existing command-ID idempotency.

#### Events/reconnect

1. Browser opens local WebSocket; native opens event stream.
2. Host subscribes before acknowledging and writes validated records.
3. Network loss closes local WebSocket without clearing Store state.
4. Existing `HttpTransport` reconnects with jitter, replays each cursor through proxied HTTP, buffers live events, and snapshots on gaps/generation changes.

#### Revocation

1. Owner sends authenticated loopback DELETE.
2. Host atomically marks revoked and increments revision.
3. Host broadcasts cancellation to active connections and owner UI refreshes.
4. Remote local WebSocket closes; all later streams resolve to `deviceRevoked`.
5. Native removes the profile/access after the sanitized revoked response and returns to onboarding. Host retains bounded revoked metadata.

### Failure Handling

| Path | Failure | Handling | User/system visibility | Test |
|---|---|---|---|---|
| Startup | companion flag absent | Do not touch Iroh/state | Existing output only | default/no-network integration |
| Startup | explicit relay/key/registry failure | Fail startup, no silent downgrade | Actionable bounded CLI error | corrupt/symlink/offline fixtures |
| Pair ticket | malformed/expired/reused | Reject before pending mutation | New-invitation guidance | unit/state-machine |
| Pair request | endpoint/HostId mismatch | Consume nothing; fail closed | Blocking identity error | integration |
| Approval | denied/expired | Terminal state; no registry entry | Mobile returns to import | state-machine/E2E |
| Ack | secure storage unavailable | Do not activate device | Unlock/storage guidance | mocked keyring |
| Request | unknown/revoked endpoint | Reset/401 or 403 before router | Pair/revoked state | integration |
| Command | envelope device mismatch | 403 before supervisor/cache | Non-retryable authorization error | dispatch counter test |
| Request | bad route/header/frame/size | Stop/reset stream | Sanitized validation/limit state | fuzz/property matrix |
| Event | receiver lag | Reset `replayRequired` | Existing reconnect banner/replay | lag integration |
| Relay/network | timeout/offline | Bound attempt; preserve state | Named host + reconnect | deterministic proxy fault |
| Host identity | key changed | Never connect to replacement | Blocking re-pair prompt | client harness |
| Protocol | major mismatch | No retry loop | Update host/app guidance | contract test |
| Revocation | active stream | Commit then close all streams | Owner sees revoked; mobile onboarding | multi-stream integration |
| Internal | handler/keyring/I/O defect | Sanitized code; local structured diagnostic | Generic retry/support action | fault injection |

Retries are owned by pairing status polling and existing web reconnect only. Pair request identity, owner decisions, ack, revoke, and command IDs are idempotent. Native transport does not silently replay arbitrary mutations.

### Security and Privacy

Trust boundaries:

1. Model/repository/tool output remains behind existing typed Serve projection.
2. Shared web content is untrusted relative to native secrets and may call only its same-origin local proxy.
3. Native proxy is trusted to hold endpoint identity and pinned host profile but cannot broaden host routes.
4. Iroh authenticates endpoint keys and encrypts QUIC end to end; relay infrastructure is an untrusted metadata observer.
5. Companion transport authenticates/binds a device before shared application dispatch.
6. Loopback owner is the only administration principal.

Threat model:

| Threat | Control |
|---|---|
| Passive/active WAN observer | Iroh authenticated encrypted QUIC and pinned endpoint ID |
| Malicious/compromised relay | No plaintext/application credentials; endpoint pin prevents impersonation; document metadata visibility |
| N0 DNS/pkarr leakage | Empty builder, explicit relay map, no discovery services/preset |
| Stolen invitation | 256-bit, 120 seconds, one request, still requires local owner approval and phrase |
| Host impersonation/rotation | Ticket and stored endpoint pin; fail closed and re-pair |
| Device impersonation | Persistent endpoint private key in OS keyring; host binds `remote_id` to registry |
| Browser/JS credential theft | Keys/profile endpoints never exposed to bundle; local cookie + CSP + exact origin/path |
| Local CSRF/rebinding | Random loopback port/cookie, exact Host/Origin/Fetch Metadata, no CORS |
| Claimed other device ID | Decode and compare before supervisor dispatch |
| Captured duplicate command | Existing device-scoped command ID cache/idempotency |
| Revoked active client | Durable registry commit, broadcast close, per-stream recheck |
| Malicious paired client | Exact typed routes/bounds; no admin/terminal; authority/trust unchanged |
| Future route accidentally exposed | Shared exact allowlist defaults deny |
| DoS | connection/stream/pairing/rate/frame/aggregate/time bounds and QUIC flow control |
| Host compromise | Out of scope: host already owns agents, tools, files, and provider credentials |
| Unlocked stolen phone | OS protection plus host revocation; no claim of protection while unlocked |

Logs include public endpoint/device IDs only when needed and may hash them; they never include tickets, poll values, secret bytes, command bodies, prompts, paths, provider payloads, or response bodies. No outbound product telemetry is added.

### Performance and Capacity

- Target one host, up to 128 retained devices, three pending pairings, four concurrent connections per device, 16 total remote connections, 32 in-flight request streams, and one event stream per connected app.
- Header 16 KiB, body chunks 64 KiB, events 1 MiB, and route aggregate limits prevent unbounded allocation.
- Host request bodies are accumulated only when existing handlers/body validation require it; responses stream and split oversized Axum chunks into protocol chunks.
- Native proxy uses bounded channels/QUIC flow control and avoids durable transcript caching.
- Device last-seen writes are throttled; event and command hot paths do not rewrite the registry.
- Exact production assets remain about 2.8 MiB and are memory-mapped/embedded once.
- No relay retry fan-out, DNS discovery, background polling, or multi-host connection pool exists in v1.

### Observability

Structured local diagnostics (using existing logging/output conventions) include category, stable sanitized code, protocol version, direct/relay class, route class, status, byte counts, duration, and retry/cancellation outcome. Pairing and registry revisions are visible, but capability values and secret material are not.

Counters/gauges: active connections by direct/relay, in-flight streams, request outcomes, rejected bounds/routes/principals, event lag resets, pairing states, registry revision, and revocations. Histograms: connect, request, bootstrap, and replay duration plus request/response bytes. No remote dashboard is required for the experimental slice; a bounded redacted diagnostics export is later rollout work.

Alerts do not apply to a local experimental process. Release review treats any secret scan, principal-binding test, replay-integrity failure, or repeated endpoint identity mismatch as a stop-ship signal.

### Migration, Rollout, and Rollback

- Registry/version starts at v1; no prior device state exists. The older LAN specification has no implemented state to migrate.
- Add shared protocol and host behind default-off config first; loopback tests must stay unchanged.
- Add native client and owner UI after black-box host harness passes.
- Bundle sync remains the existing source-of-truth process; mobile verifies the same manifests at build/startup.
- Release packages must include new path dependencies without moving them into the root workspace.
- Rollback means omit flags/ship prior extension and mobile build. Session/project/event state is untouched. Companion v1 state can remain dormant and be read again by the same version.
- Endpoint-key reset is an explicit destructive recovery operation that revokes all pairings; never auto-migrate or auto-regenerate around corruption.

### Test Strategy and Traceability

| Requirement | Components / flow | Verification |
|---|---|---|
| R1–R2 | CLI, empty builder, explicit map | parse tests, source audit, no-flag network harness |
| R3–R6 | ticket, pairing state, host/mobile keys | state-machine tests, restart/keyring harness, secret scans |
| R7 | remote command admission | mismatched principal integration with supervisor dispatch counter |
| R8–R9 | capabilities/route allowlist | paired bootstrap and terminal/admin rejection tests |
| R10–R12 | local proxy + framing | route matrix, binary/upload/export, cancellation/backpressure tests |
| R13 | existing web reconnect/replay | deterministic disconnect/lag/gap E2E |
| R14–R15 | registry/control/UI | commit-before-close and real Devices RTL/Playwright |
| R16–R17 | synchronized bundle/failures | hash gate, responsive smoke, failure-state tests |
| R18–R20 | MSRV/package/state/privacy | Rust 1.86 checks, independent build, symlink/mode/atomic tests, docs audit |

Unit tests:

- Canonical ticket encode/decode, host fingerprint, route classifier, framing bounds, malformed lengths/UTF-8/unknown fields.
- Endpoint secret wrapper redaction and exact-length persistence.
- Registry duplicate, revision, revoke, corrupt, oversized, symlink, atomic replacement, and identity-mismatch cases.
- Every pairing transition, timeout, idempotent retry, capacity, denial, ack, cancel, and secret cleanup.

Integration/contract tests:

- In-memory/direct Iroh endpoints with test relay disabled; authenticated bootstrap, all route classes, command binding, event stream, replay, upload/download bytes, cancellation, lag, and revocation.
- Explicit custom relay-map construction test and source audit banning preset calls in companion modules.
- Existing loopback transport suite unchanged plus owner-admin authorization.
- Web strict decoders/transport/device UI, CSP/external-request audit, production bundle synchronization.
- Tauri Rust client/proxy tests against host harness without requiring WebKit signing.

Manual/platform:

- macOS host to iOS and Android across different networks through n0 relay.
- Wrong pin, app/host version mismatch, locked keychain/keystore, airplane mode, foreground/background transitions, large upload/export cancellation, and revocation during event/transfer.
- Physical-device key persistence and iOS ThisDeviceOnly verification before distribution.

Required automated gates include independent backend/protocol/mobile `cargo test/check`, `cargo +1.86 check`, coding-agent Serve tests, web typecheck/lint/unit/build/boundary/CSP/bundle checks, root workspace tests relevant to CLI/dispatcher, formatting, Clippy, diff check, and package-boundary audit. Platform generation/build gates remain explicitly blocked until CocoaPods, simulator runtimes, Android SDK/Gradle, and signing are available.

### Implementation Sequence

1. **Contract slice:** add this approved artifact and WAN threat model; create shared route/framing/ticket crate with unit tests.
2. **Host identity slice:** add explicit CLI/config forwarding, private endpoint-key/registry persistence, explicit n0 map, startup/shutdown, and no-flag regression tests.
3. **Pairing slice:** implement invitation/request/status/approval/ack, owner-only APIs, principal registry, revocation, and black-box tests.
4. **Operational slice:** factor shared application router, implement QUIC HTTP/events adaptation, exact route limits, principal binding, terminal/admin rejection, and replay tests.
5. **Owner UI slice:** replace fixture-local Connected Devices behavior with typed transport/store calls, pending approval, invitation copy, and revoke.
6. **Native slice:** scaffold Tauri 2, keyring wrapper, host profile, pairing onboarding, Iroh client, authenticated local proxy, exact bundle serving, and desktop `cargo check`/unit harness.
7. **End-to-end slice:** run host plus native proxy against real Serve sessions for bootstrap, events, command, upload/resource/export, disconnect/replay, and revocation.
8. **Release/documentation slice:** update current state, architecture, native delivery, acceptance, packaging, boundary checkpoint, relay disclosure, and platform prerequisites; run complete gates.
9. **Platform lane (blocked independently):** install CocoaPods/iOS runtimes and Android SDK/Gradle, generate projects, test physical devices, then signing/distribution.

Slices 1–4 are host/protocol dependent and sequential. Native shell scaffolding can proceed after the shared protocol exists; owner UI can proceed once owner API DTOs settle. Platform tooling is independent of host correctness and must not block desktop/protocol verification.

### Open Technical Decisions

None blocking for the vertical slice. A streaming rather than bounded-buffering local HTTP response implementation is preferred for exports and is required before broad dogfood; the framing contract supports it. Custom/self-hosted relays and LAN contract reconciliation remain later designs.

### Engineering Decision Log

- 2026-08-14: Reuse cloned Axum application routes beneath separate authentication instead of duplicating handler semantics.
- 2026-08-14: Introduce a small shared companion-protocol crate because both host and native Rust must enforce identical framing and route bounds; keep Iroh/Ygg types out of it.
- 2026-08-14: Use one QUIC bidirectional stream per operation and a dedicated long-lived event record stream; rely on stream reset/stop for cancellation.
- 2026-08-14: Use endpoint public keys as device authentication credentials; do not layer a bearer token on authenticated QUIC.
- 2026-08-14: Use a native loopback proxy over custom WebView schemes/raw command replication because it naturally captures existing fetch, WebSocket, downloads, resources, and binary URLs behind one same-origin boundary.
- 2026-08-14: Keep local terminal on the existing authenticated loopback WebSocket only.
- 2026-08-14: Persist raw Iroh key bytes only through narrow wrappers and OS/Serve protected stores; no serde for private keys.
- 2026-08-14: Fail explicit companion startup rather than silently falling back to loopback-only.
