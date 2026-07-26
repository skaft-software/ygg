# Accountless LAN pairing

The long-term direction is a Syncthing-like network of trusted Ygg devices with
no required vendor account. The first networked release is intentionally
LAN-only.

This means Syncthing-like device identity and pairing, not multi-host session
replication.

## Topology

- One desktop host runs `ygg serve` and remains authoritative for its sessions.
- Phone, laptop, and desktop clients on the same LAN pair with that host.
- A client may remember several hosts but selects one active host at a time.
- Several paired clients may use the same host concurrently.
- Provider credentials and agent execution remain on the host.
- Hosts do not merge or replicate session stores in this version.

## Pairing ceremony

1. A client discovers a nearby Ygg host or enters its LAN address.
2. Discovery exposes only a user-assigned host name and stable public device
   identity, never session metadata.
3. The host creates an expiring, single-use pairing capability.
4. The user transfers it by QR code or manual phrase.
5. Both endpoints show a matching human-checkable fingerprint.
6. The host explicitly confirms the new device.
7. The host records the client's public identity and grants Full Ygg client
   access.
8. The paired client enters the ordinary Ygg interface and opens a fresh
   session.

Full client access permits viewing sessions, submitting prompts, answering
approvals, stopping work, and retrieving outputs. It never raises the host or
session's configured agent-authority ceiling. Persistent executable trust
remains host-local.

## Security requirements

- Stable cryptographic host and client identities.
- Maintained authenticated-encryption protocols; no custom cryptography.
- Mutual authentication after pairing.
- Pairing secrets are random, one-use, and short-lived.
- Authentication binds to the paired identity, not an IP address or bearer URL.
- Replay-resistant pairing and commands.
- Immediate device revocation and connection termination.
- Opaque, authenticated, bounded artifact handles.
- Bounded requests, event frames, uploads, downloads, replay, and command rate.
- No provider keys, environment secrets, permanent bearer token, unrestricted
  path, or raw process access on clients.
- Discovery is an untrusted hint and never grants access.
- An unpaired or revoked client cannot retrieve a host catalog or session data.

The local browser server remains loopback-only. LAN access is a separate
authenticated transport capability, not a relaxed bind address.

## Multi-client behavior

- Clients receive the same ordered shared events and authoritative snapshots.
- Each client keeps its own navigation, scroll position, pane state, and unsent
  draft.
- Concurrent submissions use explicit queue or steer semantics.
- Duplicate command IDs return the existing acknowledgement.
- An approval resolved by one client resolves everywhere exactly once.
- Selecting a session on one client does not change another client's view.

## Later WAN phase

The identity and transport abstraction should allow later direct WAN
connections, rendezvous, NAT traversal, and optional relay without adding a
required central account. Discovery remains separate from authentication, and
relay traffic remains end-to-end encrypted.
