# Ygg Companion (experimental)

This directory contains the native Tauri 2 companion for one authoritative
`ygg serve` host. It is a personal-use vertical slice, not a signed or
store-ready mobile release. Agents, tools, provider credentials, projects, and
the terminal remain on the host.

## Prepare this checkout

Use Node 22.13 and Rust 1.86. Build the synchronized web bundle before the host
or native shell:

```sh
npm --prefix apps/web ci
npm --prefix apps/mobile ci
npm --prefix apps/mobile run sync:web
cargo +1.86.0 build -p ygg-coding-agent --features serve --locked
```

The mobile package lock pins Tauri CLI 2.11.1. The native Rust crate is an
independent workspace under `apps/mobile/src-tauri` and uses its own lockfile.
There is intentionally no Tauri capability file: bundled content has no IPC
commands or plugin permissions. The keyring and remote transport are used only
from Rust, while the webview communicates exclusively with the app-owned
loopback origins. Adding a capability or frontend Tauri API is an authority
boundary change and requires a security review. On Android, native startup
retains Tauri/Wry's application context and initializes the shared `ndk-context`
instance before any keyring access; failure aborts startup rather than falling
back to unprotected storage.

For a local macOS shell harness, run:

```sh
npm --prefix apps/mobile run tauri -- dev
```

For a device build, install the normal Tauri platform prerequisites, then use
`npm --prefix apps/mobile run tauri -- ios init` / `ios dev` or the equivalent
`android init` / `android dev` commands. iOS requires Xcode and CocoaPods;
Android requires an SDK, NDK, and Gradle tooling. Signing, distribution, and
store release are intentionally outside this experiment.

## Start the authoritative host

Companion networking and n0 relay use are both explicit opt-ins:

```sh
./target/debug/ygg serve --companion --companion-relay n0
```

Add `--no-open` only when you can open the printed one-use loopback launch URL
locally on the host. Do not share that URL. Starting `ygg serve` without both
companion flags retains the existing loopback-only behavior and creates no
companion endpoint or state.

Keep this process running for mobile access. Its endpoint identity and device
registry persist beneath the protected Serve session state, so an ordinary host
restart keeps pairings. n0 relays can observe network addresses, timing, region,
and traffic volume; application content remains end-to-end encrypted.

## Pair once

1. In the owner browser on the host, open **Connected devices** and choose
   **Pair a device**.
2. Copy the complete one-time ticket into the native app, enter a device name,
   and choose **Request approval**.
3. Compare the verification phrase shown by the host and phone. Deny the request
   if either phrase differs.
4. Approve the matching request in the owner browser. Wait for the phone to
   confirm protected credential storage and open the app.

Tickets expire and work once. Create a new ticket after expiry or cancellation.
Pairing never grants terminal access, changes project/session authority, or lets
the phone administer other devices.

## Daily use and recovery

- Reopening or foregrounding the app reconnects to the pinned host identity.
  The client resumes from stored event cursors; replay gaps fall back to the
  host's authoritative snapshots.
- A host or relay outage is online-only: leave the app open or retry after the
  host returns. There is no phone-side agent execution or offline queue.
- An identity or protocol mismatch fails closed. Do not bypass it; verify both
  binaries came from the same checkout, then revoke/remove and pair again.
- If the app reports **restart required**, terminate it fully and reopen it
  before attempting another pairing. Startup completes any interrupted local
  credential-removal transaction.

## Revoke and remove access

For complete removal, perform both sides in this order:

1. In the host owner's **Connected devices** screen, choose **Revoke**. The host
   persists revocation before closing active connections; only this owner-only
   action revokes the authoritative device record.
2. In the app's **Settings**, choose **Open native companion settings**, then
   **Remove local companion access**. This isolated app-owned origin deletes the
   endpoint identity, pairing proof, and pinned host profile. Restart the app
   afterward.

Local removal alone does not revoke the retained host record. Conversely, after
host revocation the app still offers local removal so protected credentials can
be erased. To reconnect later, restart and complete a new owner-approved
pairing.

## Current limits

- One host only; no account, multi-host aggregation, replication, or offline
  execution.
- No remote terminal and no device administration from a paired client.
- iOS 15 and Android API 26 are the configured minimums.
- Physical-device keyring/lifecycle behavior and generated iOS/Android packages
  still require platform tooling and device validation.
- No production signing, store packaging, background-service guarantee, or
  generalized distribution support.
