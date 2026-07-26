# Ygg Serve LAN pairing

**Status:** implementation specification for the experimental LAN v1.

## Product invariant

LAN v1 is an authoritative-host system with Syncthing-like discovery and
pairing, not peer replication. One `ygg serve` process owns the session catalog,
session actors, tools, provider credentials, and durable state. Paired clients
are authenticated views and controllers of that host. When the host is offline,
its clients are offline.

The existing loopback browser remains the local owner interface. macOS, iOS,
and Android clients use the same `apps/web` interface through thin native
shells. There is no account or login.

## Scope

LAN v1 includes:

- opt-in LAN listening with `ygg serve --lan`;
- same-LAN discovery through DNS-SD/mDNS;
- explicit first-use pairing and local-host approval;
- a persistent host identity and per-device credentials;
- authenticated HTTP and WebSocket access to the existing Ygg Serve protocol;
- device listing and revocation from the loopback owner interface;
- reconnect, cursor replay, and snapshot replacement;
- thin macOS, iOS, and Android shells around the shared web application.

LAN v1 explicitly excludes:

- WAN access, relays, NAT traversal, UPnP, port forwarding, or cloud rendezvous;
- peer-to-peer session replication or multi-host conflict resolution;
- remote browser access;
- Noise, mutual TLS, WebRTC, libp2p, OAuth, or a user-account system;
- background mobile execution, push notifications, and offline transcript
  storage;
- device-to-device administration;
- automatic trust-store or CA installation;
- terminal/TUI streaming;
- any implicit expansion of Ygg tool authority. In particular, pairing does not
  enable Remote Read or raise an agent authority ceiling.

The credential and locator interfaces must not bake in LAN addressing so a
future relay can reuse host identity, device authorization, and the application
protocol without changing session semantics.

## Architecture

Ygg Serve runs two isolated transports:

```text
Local browser                     Native macOS/iOS/Android client
     |                                           |
     | loopback cookie                           | pinned TLS + device credential
     v                                           v
Loopback HTTP listener                 LAN HTTPS listener
     |                                           |
     +------------------+------------------------+
                        |
                        v
            SessionSupervisor / actors
                        |
                        v
                 authoritative Ygg App
```

### Loopback transport

The existing loopback listener remains HTTP on `127.0.0.1`/`localhost` and
continues to use:

- the one-use launch capability;
- an HttpOnly, SameSite=Strict cookie;
- exact Host, Origin, and Fetch Metadata validation;
- same-origin static web assets;
- the operational `/api/v1` routes and event WebSocket.

The loopback principal is derived from the authenticated launch cookie and is
returned by bootstrap. The browser must no longer invent an unauthenticated
device ID in local storage. Pairing and device-administration routes exist only
on this listener.

### LAN transport

The LAN listener:

- starts only when `--lan` is supplied;
- serves TLS 1.3 only;
- serves no static assets and no HTML;
- exposes the shared operational API and the pairing protocol only;
- emits no CORS allow headers and rejects browser-originated operational
  requests;
- requires a paired-device authorization header on every operational HTTP and
  WebSocket handshake;
- applies body, query, connection, and per-principal rate limits before work is
  admitted;
- admits peers only from a subnet belonging to an enabled non-loopback local
  interface.

`--lan-port 0` may request an ephemeral port because discovery publishes the
resolved port. `--lan-interface <name>` is an optional advanced override. An
explicit `--lan` startup must fail with an actionable error if no eligible
interface, TLS identity, or advertisement can be established; it must not
silently fall back to loopback-only behavior.

## Security decision

LAN v1 uses:

- TLS 1.3 with a host-local certificate authority;
- a client-pinned host CA;
- a random 256-bit credential for every paired device;
- application-layer device authentication.

This is mutual authentication but deliberately not mutual TLS:

- the client authenticates the host by its pinned CA;
- the host authenticates the client by its device credential.

Noise would duplicate TLS and require a custom encrypted framing protocol.
Mutual TLS would add CSR issuance, client-certificate provisioning, WebView
certificate behavior, and TLS-layer revocation across three native platforms.
Neither is justified for LAN v1.

The minimal new Rust dependency set belongs only to `extensions/ygg-serve`:

```toml
axum-server = { version = "0.8", features = ["tls-rustls"] }
rustls = "0.23"
rcgen = "0.14"
mdns-sd = "0.20"
if-addrs = "0.15"
ipnet = "2"
base64 = "0.22"
zeroize = "1"
```

No networking or cryptographic dependency enters a Ygg core crate.

## Host identity and persistent state

The existing stable `HostId` is retained. Its cryptographic network identity is
the tuple `(HostId, SHA-256(host CA SPKI))`. Discovery advertises both values,
and a paired client stores both.

Serve-owned LAN state lives below the already protected Serve state directory:

```text
<session-root>/.serve/lan-v1/
  host-ca-key.der       mode 0600
  host-ca-cert.der      mode 0644
  server-key.der        mode 0600
  server-cert.der       mode 0644
  devices-v1.json       mode 0600
```

The directory is mode `0700`. Creation and opening must reject symlinks and
non-regular files. Private material is opened without following symlinks.

The host CA is created once. A server certificate is signed by that CA and may
be renewed without re-pairing. If a device registry exists but the CA or its key
is missing, malformed, or mismatched, startup fails closed. Identity is never
silently regenerated.

`devices-v1.json` is versioned and contains:

```json
{
  "version": 1,
  "revision": 12,
  "devices": [
    {
      "id": "device-example",
      "name": "Achu's iPhone",
      "platform": "ios",
      "tokenHash": "sha256:...",
      "pairedAtMs": 0,
      "lastSeenAtMs": 0,
      "revokedAtMs": null
    }
  ]
}
```

The host stores only a SHA-256 digest of each 256-bit device secret and compares
digests in constant time. Registry updates use a sibling temporary file,
`sync_all`, atomic rename, and directory sync. `lastSeenAtMs` persistence is
throttled to avoid a write per request.

Native clients store the CA certificate, host identity, device ID, endpoint
hints, and device secret in:

- Apple Keychain with a ThisDeviceOnly accessibility class;
- Android Keystore-backed encrypted storage.

Credentials never enter JavaScript, URLs, WebSocket subprotocols, local storage,
logs, crash messages, or analytics.

## Discovery

The host advertises:

```text
service type: _ygg._tcp.local.
TXT:
  v=1
  id=<host-id>
  fp=<base64url SHA-256 host CA SPKI>
  pair=0|1
```

The SRV record carries the LAN port. The TXT record is bounded, contains no
credential, and is only a location hint. A client never trusts an mDNS
fingerprint until the pairing ceremony or compares it in place of its stored
pin.

Apple clients declare `_ygg._tcp` in `NSBonjourServices` and provide
`NSLocalNetworkUsageDescription`. Android uses `NsdManager` and handles the
platform local-network permission model. Discovery runs only while the chooser
or reconnect flow needs it.

On reconnect, a client tries the last endpoint briefly, then resolves the stored
HostId through discovery. Address changes are accepted only when the TLS chain
matches the stored host CA.

## Principals and authorization

Transport authentication produces:

```rust
pub struct AuthenticatedClient {
    pub device_id: DeviceId,
    pub display_name: String,
    pub kind: ClientKind,
    pub permissions: Vec<DevicePermission>,
}

pub enum ClientKind {
    LoopbackOwner,
    Paired,
}

pub enum DevicePermission {
    SessionsRead,
    SessionsControl,
    DeviceAdministration,
}
```

The loopback principal has all three permissions. A LAN-v1 paired principal has
`SessionsRead` and `SessionsControl` only.

LAN operational requests authenticate with:

```text
Authorization: Ygg-Device v1:<device-id>:<base64url-secret>
```

The authorization middleware:

1. parses and bounds the header;
2. resolves the non-revoked registry entry;
3. hashes and constant-time compares the secret;
4. attaches `AuthenticatedClient` to the request;
5. attributes rate limiting and active connections to that principal.

`HostCommandEnvelope` and `SessionCommandEnvelope` retain their `device_id`
field for idempotency. Before the supervisor sees a command, the route must
verify that the envelope device ID equals the authenticated principal. A
mismatch is a non-retryable 403 and causes no mutation.

`HostBootstrap` adds:

```rust
pub authenticated_client: AuthenticatedClient
```

Host capabilities are principal-sensitive. `connected_devices` and pairing
administration are exposed only to the loopback owner. `lan_clients` is true
when the LAN listener is healthy.

## Pairing state machine

Pairing is closed by default. At most one pairing window and three pending
requests exist at once.

```text
Closed
  | owner opens (120 seconds)
  v
Open(ticket)
  | valid client request
  v
Pending(request)
  | owner denies ---------> Denied
  | owner approves
  v
Approved(plaintext credential held in memory)
  | client durable-storage acknowledgement
  v
Consumed

Open/Pending/Approved -- deadline --> Expired
Open/Pending/Approved -- owner closes --> Cancelled
```

All terminal states reject further mutation. Exact retries with the same
request identity return the same state. A consumed, expired, denied, or
cancelled ticket cannot create another request.

### Owner ceremony

1. The loopback owner selects **Connected devices → Pair a device**.
2. The host creates one memory-only 256-bit ticket with a 120-second TTL and a
   separate 100-bit Crockford Base32 manual alias mapped to that ticket.
3. The UI displays:
   - a QR URI containing protocol, HostId, endpoint, exact current server-leaf
     pin, and ticket;
   - a bounded manual ticket;
   - a six-word fingerprint derived from HostId and host CA SPKI;
   - the selected interface and expiration countdown.
4. The client scans the QR or selects the discovered host and enters the manual
   ticket.
5. Manual pairing requires the user to confirm that the six words shown by both
   devices match before approval.
6. The native client submits the ticket, device presentation metadata, app
   version, and a random client nonce over the pinned TLS connection.
7. The loopback owner sees the pending device and must explicitly allow or deny
   it.
8. On approval, the host allocates a random `DeviceId` and 256-bit device
   secret, persists only its digest, and exposes the plaintext credential only
   through the authenticated pending request.
9. The client stores the credential and host CA, then acknowledges durable
   storage.
10. Acknowledgement zeroizes the in-memory plaintext credential and consumes
    the ticket.

If the approval response is lost, polling with the request's independent
high-entropy poll token returns the same approved credential until
acknowledgement or expiry. The poll token and device credential are never
placed in query strings.

Pairing protections:

- five invalid attempts per source address per minute;
- one active window, three pending requests, and one request per ticket;
- bounded names and versions treated only as untrusted presentation data;
- no ticket persistence or logging;
- no approval from a paired client;
- no continue-anyway path after a fingerprint mismatch.

## HTTP and WebSocket protocol

### Existing operational routes

Both authenticated transports retain:

```text
GET  /api/v1/bootstrap
GET  /api/v1/sessions/{sessionId}
GET  /api/v1/sessions/{sessionId}/replay
POST /api/v1/commands/host
POST /api/v1/commands/session
WS   /api/v1/events
```

The loopback transport authenticates these routes with its launch cookie. The
LAN transport authenticates them with `Ygg-Device`.

### Loopback-owner routes

These routes are not mounted by the LAN router:

```text
GET    /api/v1/devices
POST   /api/v1/pairing/open
GET    /api/v1/pairing/state
POST   /api/v1/pairing/requests/{requestId}/decision
DELETE /api/v1/pairing
DELETE /api/v1/devices/{deviceId}
```

`POST /api/v1/pairing/open` is idempotent while a window is open and returns
the current ticket. A decision body is exactly:

```json
{ "decision": "approve" }
```

or:

```json
{ "decision": "deny" }
```

Revocation is idempotent. The owner cannot revoke its loopback principal through
the device endpoint.

### LAN pairing routes

These routes are not mounted by the loopback router:

```text
GET  /pair/v1/info
POST /pair/v1/requests
POST /pair/v1/requests/{requestId}/status
POST /pair/v1/requests/{requestId}/ack
```

`/pair/v1/info` exposes only the bounded host descriptor, protocol version,
certificate fingerprints, and whether pairing is open.

The request contains:

```rust
pub struct PairingRequestInput {
    pub protocol: u16,
    pub ticket: PairingSecret,
    pub client_nonce: String,
    pub device: PairingDeviceClaim,
    pub observed_host_id: HostId,
    pub observed_server_pin: String,
}
```

`PairingSecret` is an extension-private, zeroizing 32-byte value with explicit
base64url/Crockford decoding and no `Debug` or `Display` implementation.

The creation response returns a `request_id`, independent `poll_token`,
fingerprint words, state, and expiry. Status requires the poll token in its JSON
body and returns exactly one of:

```rust
pub enum PairingStatus {
    Pending { request: PairingRequestSummary },
    Approved {
        device: DeviceSummary,
        credential: DeviceCredential,
        host_ca_der: String,
        endpoint: String,
    },
    Denied,
    Expired,
    Cancelled,
}
```

The acknowledgement body proves the approved request ID and poll token. No
secret-bearing type implements `Debug`, `Display`, or serialization outside its
explicit response boundary.

### Device DTOs and errors

```rust
pub struct DeviceSummary {
    pub id: DeviceId,
    pub name: String,
    pub platform: DevicePlatform,
    pub paired_at_ms: u64,
    pub last_seen_at_ms: Option<u64>,
    pub connection: DeviceConnectionState,
    pub revoked_at_ms: Option<u64>,
}

pub struct DeviceCatalog {
    pub revision: u64,
    pub devices: Vec<DeviceSummary>,
}
```

New sanitized error codes:

```text
pairingClosed
pairingExpired
pairingTicketInvalid
pairingCapacity
pairingDenied
hostIdentityChanged
deviceCredentialInvalid
deviceRevoked
deviceIdentityMismatch
notOnAllowedLan
protocolMismatch
```

Missing or invalid credentials return 401. A known revoked device or an
envelope/principal mismatch returns 403. Capacity and rate limits remain
retryable. Identity and protocol mismatch are never retried automatically.

## Native bridge

The native applications bundle the exact `apps/web` production output and load
it from one app-owned origin. Web content does not connect directly to the LAN
host.

The bridge accepts only:

```ts
type BridgeRequest = {
  id: string;
  method: "GET" | "POST";
  path: string;
  body?: string;
};

type BridgeResponse = {
  id: string;
  status: number;
  body: string;
};

type BridgeEvent =
  | { type: "open" }
  | { type: "message"; body: string }
  | { type: "close"; code: number }
  | { type: "error"; code: string };
```

Allowed paths are the exact operational routes above. The bridge rejects:

- absolute URLs;
- scheme or authority components;
- path traversal or encoded separators;
- arbitrary headers;
- unrecognized methods and routes;
- messages from a non-main frame or any origin other than the bundled app.

The bridge itself:

- chooses the stored host endpoint;
- validates the TLS chain against the stored host CA and HostId;
- adds the device authorization header;
- performs one HTTP or WebSocket attempt;
- returns raw bounded JSON to TypeScript;
- never retries, projects events, or interprets commands.

`NativeBridgeTransport` in the web application reuses the existing wire
projection, encoded-command cache, cursor tracking, replay, and event buffering.
Native clients therefore cannot drift into a second Ygg protocol
implementation.

Apple uses `URLSession`, `URLSessionWebSocketTask`, a narrowly scoped
`WKScriptMessageHandler`, and an app-owned `WKURLSchemeHandler`. Android uses
OkHttp, `WebViewAssetLoader`, and `WebViewCompat.addWebMessageListener` with the
single asset origin. Android must not bypass `onReceivedSslError`.

## Reconnect and failure behavior

TypeScript remains the sole reconnect owner:

1. retain the visible snapshot, draft, attachments, and exact serialized
   in-flight command;
2. retry the native or browser event connection with jittered delays of
   approximately 250 ms, 500 ms, 1 s, 2 s, then 5 s maximum while foregrounded;
3. on reopen, request replay for every known session cursor;
4. buffer newly received live events while replay runs;
5. apply replay in cursor order, then buffered host-sequence order;
6. replace with the authoritative snapshot on a replay gap or actor-generation
   change;
7. resend an unacknowledged mutation only with its original command ID and
   exact serialized body.

Required failure surfaces:

- **No hosts found:** same-Wi-Fi, local-network permission, `--lan`, and manual
  address guidance.
- **Permission denied:** platform Settings action.
- **Pairing closed:** exact instruction to open Connected devices on the host.
- **Pending:** device name, matching fingerprint words, countdown, and Cancel.
- **Denied or expired:** preserve host selection and allow a fresh ceremony.
- **Disconnected:** non-modal stale-state banner; do not clear transcript or
  draft.
- **Host restarted:** reconnect and replay without re-pairing.
- **Host identity changed:** blocking warning and explicit re-pairing; never
  trust automatically.
- **Device revoked:** return to pairing while retaining the unsent draft.
- **Wrong LAN or VPN:** display the intended host and last endpoint.
- **Protocol mismatch:** identify whether the host or client must be updated.

There is no durable native offline transcript cache in v1.

## Revocation

Only the loopback owner can revoke a device. Revocation:

1. atomically records `revokedAtMs` and removes the usable token digest;
2. increments the device-catalog revision;
3. closes all active HTTP/WebSocket connection tasks attributed to the device;
4. makes all subsequent requests return `deviceRevoked`;
5. retains bounded device metadata for owner visibility.

An active-connection registry is indexed by `DeviceId`. WebSocket and long-lived
request guards unregister themselves on drop. Revocation waits for the registry
commit before closing connections so reconnect cannot race ahead of durable
state.

## Module and ownership boundaries

All substantive implementation remains under the optional extension:

```text
extensions/ygg-serve/src/
  auth.rs
  device.rs
  discovery.rs
  identity.rs
  lan_policy.rs
  pairing.rs
  transport/
    mod.rs
    common.rs
    loopback.rs
    lan.rs
    routes.rs
```

Existing extension files change only as follows:

- `model.rs`: authenticated-client and public device DTOs;
- `bounds.rs`: pairing, header, registry, and catalog bounds;
- `error.rs`: sanitized LAN and pairing errors;
- `lib.rs`: public exports;
- `fixtures/`: authenticated-bootstrap, pairing, and device-catalog goldens;
- `tests/lan_transport.rs`, `tests/pairing.rs`, and `tests/security.rs`: black-box
  contracts.

Web ownership:

```text
apps/web/src/native/
  bridge.ts
  NativeBridgeTransport.ts
  bridge.test.ts
```

`protocol.ts`, `wire.ts`, `transport.ts`, `store.ts`, and
`components/Devices.tsx` gain the client/device/pairing projections and failure
states.

Native ownership:

```text
apps/apple/
  Shared/
    HostDiscovery.swift
    PairingClient.swift
    CredentialStore.swift
    PinnedSession.swift
    WebBridge.swift
    YggWebView.swift
  macOS/
  iOS/

apps/android/app/src/main/
  java/.../
    HostDiscovery.kt
    PairingClient.kt
    CredentialStore.kt
    PinnedClient.kt
    WebBridge.kt
    YggWebView.kt
  assets/web/
```

Build phases copy `apps/web/dist` into native resources. Generated copies are
not edited by hand.

The only unavoidable `ygg-coding-agent` seams are:

- `src/cli.rs`: `--lan`, `--lan-port`, and `--lan-interface`;
- `src/main.rs`: pass those options;
- `src/extensions/serve.rs`: provide the session root and host descriptor and
  start the extension runtime;
- `Cargo.toml`: retain `ygg-serve-backend` behind the single optional `serve`
  feature.

Core agent, AI, session, TUI, and tool crates do not learn about discovery,
pairing, TLS, or devices.

## Threat model

| Threat | Required control |
| --- | --- |
| Passive LAN observer | TLS 1.3 for every LAN byte |
| Active pairing MITM | QR server pin or user-confirmed six-word fingerprint |
| Spoofed mDNS | Discovery is a hint; stored host CA is authoritative |
| DNS rebinding or browser CSRF | Separate listeners, no LAN CORS, native-only credential header, strict loopback origin checks |
| Ticket guessing | 256-bit QR ticket, short TTL, single use, rate limits |
| Credential leakage | Keychain/Keystore, digest-only host storage, no JS/URL/log exposure |
| Captured duplicate command | Existing device-scoped command-ID idempotency |
| Revoked live client | Durable revocation followed by active-connection cancellation |
| Malicious paired client | No device administration; agent authority remains independently bounded |
| Host key loss or replacement | Fail closed; explicit re-pairing after intentional reset |
| Routed or WAN exposure | local-interface subnet admission and no rendezvous/port mapping |
| LAN denial of service | pre-auth limits, bounded payloads, pending-request and connection caps |
| Stolen unlocked device | host revocation; OS secure storage limits extraction |
| Host compromise | Out of scope: the authoritative host already owns sessions, tools, and credentials |

## Acceptance matrix

All `R`, `N`, `W`, `A`, and `D` tests are release-blocking. Physical `E` tests
are required before distributing a signed LAN build.

| ID | Runner | Acceptance |
| --- | --- | --- |
| R01 | Rust unit | Identity survives restart and private file modes are correct |
| R02 | Rust unit | Symlinked identity, key, and registry paths fail closed |
| R03 | Rust unit | Pairing follows every specified transition and terminal states are final |
| R04 | Rust unit | Device digest authenticates only the matching non-revoked principal |
| R05 | Rust unit | Revoked and envelope-mismatched device identities are rejected before mutation |
| R06 | Rust unit | IPv4 and IPv6 subnet fixtures admit only eligible LAN peers |
| R07 | Rust unit | Interrupted registry replacement leaves either the old or new valid document |
| R08 | Rust golden | Every new bootstrap, device, pairing, and error shape round-trips and validates |
| N01 | Rust integration | LAN listener rejects plaintext HTTP |
| N02 | Rust integration | Operational routes reject missing and invalid credentials |
| N03 | Rust integration | Pairing routes reject requests while pairing is closed |
| N04 | Rust integration | Open, request, approve, poll, acknowledge yields one durable device |
| N05 | Rust integration | Wrong, expired, and reused tickets are rejected |
| N06 | Rust integration | Bootstrap reports the exact authenticated device principal |
| N07 | Rust integration | Envelope/principal mismatch returns 403 with no supervisor dispatch |
| N08 | Rust integration | Revocation closes the event socket and blocks reconnect |
| N09 | Rust integration | Restart preserves the CA, HostId binding, and paired credential |
| N10 | Rust integration | Replaced CA is rejected by the native-client harness |
| N11 | Rust integration | Disconnect plus replay produces no missing or duplicate session item |
| N12 | Rust integration | LAN API emits no CORS grant and rejects browser Origin/OPTIONS use |
| N13 | Rust integration | Logs, errors, and persistent registry contain no plaintext secret |
| N14 | mDNS integration | A custom-port test publisher is discovered, updated, and withdrawn |
| W01 | Vitest | Native bridge accepts only the exact method/path allowlist |
| W02 | Vitest | Device credential never enters a JS request or event payload |
| W03 | Vitest | Disconnect/rejection preserves draft and exact encoded command |
| W04 | Vitest | Reconnect buffers live events until replay settles |
| W05 | RTL | Loopback owner sees pairing/revocation; paired client does not |
| W06 | Playwright | Existing loopback session journeys remain green |
| A01 | XCTest | Apple credential is ThisDeviceOnly and survives relaunch |
| A02 | XCTest | Matching host CA succeeds and a changed CA fails |
| A03 | XCTest | Apple bridge rejects wrong origin and non-main-frame messages |
| A04 | XCTest | Apple local-network denied, empty, found, and lost states render correctly |
| D01 | Android unit | Keystore-backed credential survives relaunch |
| D02 | Android instrumented | Matching private CA succeeds and a changed CA fails |
| D03 | Android instrumented | Android bridge exists only on the exact asset origin |
| D04 | Android instrumented | Android NSD denied, empty, found, and lost states render correctly |
| E01 | Physical | Mac host and iPhone pair, stream, disconnect, replay, and revoke |
| E02 | Physical | Mac host and Android device complete the same lifecycle |
| E03 | Physical | Mac host and macOS client complete the same lifecycle |
| E04 | Physical | Two paired clients observe one authoritative ordered session |
| E05 | Physical | A client on a different subnet cannot discover or use the host |
| E06 | Physical | Host restart reconnects paired clients without re-pairing |

Required automated commands:

```console
cargo test --manifest-path extensions/ygg-serve/Cargo.toml
(cd apps/web && npm run typecheck && npm run lint && npm test -- --run && npm run build)
xcodebuild test -project apps/apple/Ygg.xcodeproj -scheme Ygg -destination 'platform=macOS'
xcodebuild test -project apps/apple/Ygg.xcodeproj -scheme Ygg -destination 'platform=iOS Simulator,name=iPhone 17'
(cd apps/android && ./gradlew testDebugUnitTest connectedDebugAndroidTest)
```

## Build order

1. Add host identity, authenticated principals, device registry, bounds, and
   golden contracts.
2. Split common, loopback, and LAN routers; add TLS and subnet admission.
3. Add the pairing state machine and loopback-owner administration.
4. Add mDNS advertisement and discovery-state reporting.
5. Bind bootstrap and every command envelope to the authenticated principal.
6. Implement the real Connected devices and pairing UI.
7. Factor shared TypeScript replay logic and implement `NativeBridgeTransport`.
8. Build the shared Apple shell, then the macOS and iOS targets.
9. Build the Android shell.
10. Pass automated, adversarial, multi-client, physical-device, and signed-build
    gates.

The LAN listener must not ship before the existing optional Serve feature,
embedded web payload, and installed-binary path are independently
release-valid. LAN functionality remains default-off until the full acceptance
matrix is green.
