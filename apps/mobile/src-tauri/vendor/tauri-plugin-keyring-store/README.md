<p align="center">
  <img src="assets/docs-logo.svg" width="128" height="128" alt="tauri-plugin-keyring-store logo" />
</p>

[![npm version](https://img.shields.io/npm/v/tauri-plugin-keyring-store-api/latest?style=for-the-badge)](https://www.npmjs.com/package/tauri-plugin-keyring-store-api)
[![Crates.io](https://img.shields.io/crates/v/tauri-plugin-keyring-store?style=for-the-badge)](https://crates.io/crates/tauri-plugin-keyring-store)
[![Documentation](https://img.shields.io/badge/docs-docs.rs-blue?style=for-the-badge)](https://docs.rs/tauri-plugin-keyring-store/)
[![GitHub issues](https://img.shields.io/github/issues/s00d/tauri-plugin-keyring-store?style=for-the-badge)](https://github.com/s00d/tauri-plugin-keyring-store/issues)
[![GitHub stars](https://img.shields.io/github/stars/s00d/tauri-plugin-keyring-store?style=for-the-badge)](https://github.com/s00d/tauri-plugin-keyring-store/stargazers)
[![Donate](https://img.shields.io/badge/Donate-Donationalerts-ff4081?style=for-the-badge)](https://www.donationalerts.com/r/s00d88)

# Tauri Plugin Keyring Store

Store secrets and wallet-style procedures using the **OS credential store** (macOS Keychain, Windows Credential Manager, Linux Secret Service, Android Keystore, iOS Data Protection). The guest API mirrors [`tauri-plugin-stronghold`](https://github.com/tauri-apps/plugins-workspace/tree/v2/plugins/stronghold) sessions, clients, store, vault, and crypto procedures — but **there is no encrypted snapshot file**: everything maps to hashed keyring entries under your app **service** name (defaults to the Tauri bundle identifier).

---

## Table of contents

1. [Features](#features)
2. [Platform support](#platform-support)
3. [iOS Data Protection](#ios-data-protection)
4. [Installation](#installation)
5. [Usage](#usage)
6. [Direct account API](#direct-account-api-bulk-exists-naming-backup)
7. [Cargo features](#cargo-features)
8. [Permissions](#permissions)
9. [Relationship to Stronghold](#relationship-to-tauri-plugin-stronghold)
10. [Development](#development)
11. [Testing](#testing)
12. [Contributing](#contributing)
13. [Partners](#partners)
14. [License](#license)

---

## Features

- **Cross-platform keyring** via [`keyring-core`](https://crates.io/crates/keyring-core) `1.x` and official backend crates (native stores only — no silent in-memory fallback).
- **Rust-first API**: `app.keyring()` exposes [`KeyringPlugin`] with [`KeyringStore`] for backend code without IPC.
- **Stronghold-shaped JS API**: [`KeyringSession`](guest-js/index.ts), [`KeyringClient`](guest-js/index.ts), [`KeyringStoreView`](guest-js/index.ts), [`KeyringVault`](guest-js/index.ts) + SLIP10 / BIP39 / Ed25519 procedures when the `crypto` feature is enabled.
- **Optional `crypto` feature** (default): SLIP10/BIP39/Ed25519 via [`iota-crypto`](https://crates.io/crates/iota-crypto); secrets stored as Base64 in the OS vault.

---

## Platform support

| Platform | Backend |
|----------|---------|
| macOS | Login Keychain |
| iOS | Protected (Data Protection) Keychain |
| Windows | Credential Manager |
| Linux | Secret Service (DBus; `crypto-rust` — no host OpenSSL required to **build**) |
| Android | Android Keystore + SharedPreferences |

Linux desktops need a Secret Service (e.g. GNOME Keyring / KWallet). Headless CI often has no user session — avoid relying on the live keyring there (see [Testing](#testing)). On Android, transitive deps may pull OpenSSL; your app may need `openssl-sys` with `vendored` for cross-builds (see Subly-style setups).

---

## iOS Data Protection

On iOS the plugin uses the **protected** Keychain backend. **0.2.0** writes new secrets with **`AfterFirstUnlock`** by default so background tasks can read them after the first unlock of the day, even if the screen is locked again.

| Layer | Responsibility |
|-------|----------------|
| **This plugin** | `keyring-core`, iOS `access-policy` on write, `availability()`, `Error::KeychainLocked`, `*_for_background` helpers |
| **Host app** (e.g. Subly) | Service name, `OnceLock<KeyringStore>`, map `Error` → app errors, call `get_password_for_background` from sync |

### Write policy

```rust,no_run
use tauri_plugin_keyring_store::{Builder, WriteAccessibility};

// Plugin init (Tauri)
Builder::new()
    .ios_write_accessibility(WriteAccessibility::AfterFirstUnlock)
    .build();

// Or on a standalone KeyringStore
let store = KeyringStore::new("com.example.app")
    .with_write_accessibility(WriteAccessibility::AfterFirstUnlock);
```

Reads use `Entry::new` (same service/account). Writes use `Entry::new_with_modifiers` with `access-policy: after-first-unlock` on iOS only. macOS Login Keychain ignores `WriteAccessibility` for writes.

**Migration:** entries created before 0.2.0 may still use `WhenUnlocked` until the user re-saves them (`set_password` / `set_bytes`). There is no bulk rekey command in this release.

### Locked device vs real errors

- Foreground: `get_password` → `Err(Error::KeychainLocked)` when the device is locked.
- Background sync: `get_password_for_background` → `Ok(None)` when [`availability()`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/struct.KeyringStore.html#method.availability) is `Locked`.
- `exists_nonempty_for_background` → `Ok(false)` when locked.

```rust,no_run
use tauri_plugin_keyring_store::{Error, KeyringAvailability, KeyringStore};

# fn example(store: &KeyringStore) -> Result<(), Error> {
match store.availability() {
    KeyringAvailability::Available => { /* normal UI read */ }
    KeyringAvailability::Locked => { /* prompt unlock or defer */ }
}
let secret = store.get_password_for_background("oauth-token")?;
# Ok(())
# }
```

IPC commands are unchanged in 0.2.0; Rust backends can use [`KeyringStore`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/struct.KeyringStore.html) directly without new Tauri commands.

---

## Installation

### Automatic (recommended)

From your **Tauri app root** (where `package.json` and the `tauri` script live):

```bash
pnpm run tauri add keyring-store
```

The CLI wires the Rust crate into `src-tauri` and adds the [`tauri-plugin-keyring-store-api`](https://www.npmjs.com/package/tauri-plugin-keyring-store-api) npm package when needed.

Other package managers:

```bash
npm run tauri add keyring-store
yarn tauri add keyring-store
```

With the CLI installed via Cargo: `cargo tauri add keyring-store`.

### Manual — Rust (`src-tauri`)

```bash
cd src-tauri
cargo add tauri-plugin-keyring-store
```

Or in `Cargo.toml`:

```toml
tauri-plugin-keyring-store = "0.2.0"
```

Disable the SLIP10/BIP39/Ed25519 stack (storage + backup IPC only):

```toml
tauri-plugin-keyring-store = { version = "0.2.0", default-features = false }
```

### Manual — JavaScript

```bash
pnpm add tauri-plugin-keyring-store-api
```

You still need `.plugin(tauri_plugin_keyring_store::init())` (or [`Builder`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/struct.Builder.html)) in Rust.

---

## Usage

### Backend

```rust
fn main() {
  tauri::Builder::default()
    .plugin(tauri_plugin_keyring_store::init())
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
```

Custom **service** name (defaults to `identifier` in `tauri.conf.json`):

```rust
tauri_plugin_keyring_store::Builder::new()
  .service("com.mycompany.myapp.credentials")
  .build()
```

### Rust — access the store from commands / plugins

```rust
use tauri::Manager;
use tauri_plugin_keyring_store::KeyringExt;

#[tauri::command]
fn save_api_token(app: tauri::AppHandle, token: String) -> Result<(), String> {
  app.keyring().store
    .set_password("manual.example.token", &token)
    .map_err(|e| e.to_string())
}
```

Sessions opened from the frontend (`initialize`) are tracked separately; low-level [`KeyringStore`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/struct.KeyringStore.html) calls use whatever account string you pass.

### Snapshot path (first argument of `KeyringSession.load`)

This string is **not** a path to a file on disk on **any** OS (not the macOS Keychain file, not a Windows “vault” path, not a Linux D-Bus socket path). It is a **logical session id**: the plugin hashes it together with client / vault / record names into stable OS keyring **account** strings under your app **service** (default: Tauri bundle identifier).

| You choose | Effect |
|------------|--------|
| Same string on every platform | Same secrets namespace everywhere (typical). |
| Different strings | Different isolated namespaces (e.g. per user or per “wallet”). |
| Looks like a path, e.g. `'/wallet/main'` | Fine — purely a label; no requirement that the folder exists. |

Use stable ASCII-ish identifiers for portability. The second argument is the Stronghold-compatible **password**: it is **not** used to unlock a snapshot file here; Rust **zeroizes** it. Use `''` or any placeholder if you are not migrating from Stronghold.

### Frontend — minimal flow

```typescript
import { KeyringSession } from 'tauri-plugin-keyring-store-api'

const session = await KeyringSession.load('/wallet/main', '')
const client = await session.createClient('main')
await client.getStore().insert('prefs', [...new TextEncoder().encode('{}')])
await session.unload()
```

### JavaScript API reference

Invokes use `plugin:keyring-store|<command>`. SLIP10 / BIP39 / Ed25519 helpers on [`KeyringVault`](guest-js/index.ts) require the Rust crate’s **`crypto`** feature (enabled by default).

#### `ping`

```typescript
import { ping } from 'tauri-plugin-keyring-store-api'

const value = await ping('hello') // string | null
```

#### `KeyringSession`

| Method | Purpose |
|--------|---------|
| `KeyringSession.load(snapshotPath, password)` | Registers the session (`initialize` IPC). |
| `session.unload()` | Drops session tracking (`destroy`). |
| `session.createClient(client)` | First-time client namespace (`create_client`). |
| `session.loadClient(client)` | Existing client (`load_client`). |
| `session.save()` | **No-op** on keyring (`save` IPC for Stronghold parity). |

```typescript
import { KeyringSession } from 'tauri-plugin-keyring-store-api'

const session = await KeyringSession.load('/app/secrets', '')
const created = await session.createClient('desktop')
const again = await session.loadClient('desktop')
await session.save()
await session.unload()
```

#### `KeyringClient`

| Method | Purpose |
|--------|---------|
| `client.getStore()` | JSON-like byte records (`get_store_record` / `save_store_record` / `remove_store_record`). |
| `client.getVault(name)` | Binary vault + crypto procedures (`save_secret` / `remove_secret` / `execute_procedure`). |

#### `KeyringStoreView` (from `client.getStore()`)

| Method | Purpose |
|--------|---------|
| `get(key)` | Read bytes or `null`. |
| `insert(key, value, lifetime?)` | Write bytes; `lifetime` is ignored (Stronghold compat). |
| `remove(key)` | Delete record; returns previous bytes or `null`. |

```typescript
const store = client.getStore()
const raw = await store.get('prefs')
await store.insert('prefs', [...new TextEncoder().encode('{}')])
await store.remove('prefs')
```

#### `Location` and `KeyringVault`

Build locations for vault records and procedure outputs:

```typescript
import { Location } from 'tauri-plugin-keyring-store-api'

const generic = Location.generic('WALLET', 'seed.bin')
const row = Location.counter('WALLET', 0)
```

| `KeyringVault` method | IPC / behavior |
|----------------------|----------------|
| `insert(recordPath, secret)` | `save_secret` |
| `remove(location)` | `remove_secret` (pass `Location.generic` or `Location.counter`) |
| `generateSLIP10Seed(output, sizeBytes?)` | `execute_procedure` SLIP10Generate |
| `deriveSLIP10(chain, 'Seed' \| 'Key', src, output)` | SLIP10Derive |
| `recoverBIP39(mnemonic, output, passphrase?)` | BIP39Recover |
| `generateBIP39(output, passphrase?)` | BIP39Generate |
| `getEd25519PublicKey(privateKeyLocation)` | PublicKey (Ed25519) |
| `signEd25519(privateKeyLocation, msg)` | Ed25519Sign (`msg` is UTF-8) |

```typescript
import { KeyringSession, Location } from 'tauri-plugin-keyring-store-api'

const session = await KeyringSession.load('/vault-a', '')
const vault = (await session.createClient('c1')).getVault('PRIMARY')
const out = Location.generic('PRIMARY', 'slip10-master')
await vault.generateSLIP10Seed(out, 32)
await session.unload()
```

Binary vault records (not the procedure helpers above):

```typescript
import { KeyringSession, Location } from 'tauri-plugin-keyring-store-api'

const session = await KeyringSession.load('/vault-a', '')
const vault = (await session.createClient('c1')).getVault('SECRETS')
await vault.insert('blob.bin', [0xde, 0xad])
await vault.remove(Location.generic('SECRETS', 'blob.bin'))
await session.unload()
```

#### Naming helpers

```typescript
import { joinKeyPrefix, splitKeyPrefix, KEYRING_PREFIX_SEPARATOR } from 'tauri-plugin-keyring-store-api'

const account = joinKeyPrefix('billing', 'stripe_sk')
const [prefix, name] = splitKeyPrefix(account)
void KEYRING_PREFIX_SEPARATOR // '.'
```

---

## Direct account API (bulk, exists, naming, backup)

Raw **account** strings are the OS keyring entry names under your app **service** (defaults to the bundle identifier). These commands avoid session hashing — useful for app-controlled keys.

| IPC command | Purpose |
|-------------|---------|
| `get_passwords` | Read many UTF-8 secrets (parallel `Vec`, max **256** accounts per call). |
| `set_passwords` | Write many `{ account, secret }` pairs. |
| `delete_passwords` | Delete many accounts. |
| `password_exists` | `true` if a non-empty secret exists (`exists_nonempty`). |
| `export_passwords_plain` / `import_passwords_plain` | JSON backup blob over IPC. |
| `export_passwords_encrypted` / `import_passwords_encrypted` | Argon2id + ChaCha20-Poly1305 envelope (always compiled; independent of the `crypto` feature). |

**Naming (application convention):** use `prefix.name` with a single dot — helpers [`join_prefix`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/fn.join_prefix.html) / [`split_prefixed`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/fn.split_prefixed.html) in Rust, and `joinKeyPrefix` / `splitKeyPrefix` in guest-js (see [Usage → Naming helpers](#naming-helpers)). The OS keyring still does **not** support listing by prefix; keep your own index of logical keys if needed.

**Security — plaintext backup:** `export_passwords_plain` / `import_passwords_plain` move secrets **in the clear** across IPC to the webview. Use only in trusted UI flows, or prefer `export_passwords_encrypted` / disk encryption.

### Guest-js examples

```typescript
import {
  getPasswords,
  setPasswords,
  deletePasswords,
  passwordExists,
  exportPasswordsPlain,
  importPasswordsPlain,
  exportPasswordsEncrypted,
  importPasswordsEncrypted,
  joinKeyPrefix,
} from 'tauri-plugin-keyring-store-api'

const account = joinKeyPrefix('app', 'api_token')

await setPasswords([{ account, secret: 'secret-value' }])
const values = await getPasswords([account]) // (string | null)[]
const exists = await passwordExists(account)

const plain = await exportPasswordsPlain([account])
await importPasswordsPlain(plain)

const enc = await exportPasswordsEncrypted([account], 'user-passphrase')
await importPasswordsEncrypted(enc, 'user-passphrase')

await deletePasswords([account])
```

---

## Cargo features

| Feature | Default | Description |
|---------|---------|-------------|
| `crypto` | yes | SLIP10 / BIP39 / Ed25519 `execute_procedure` via [`iota-crypto`](https://crates.io/crates/iota-crypto). Encrypted backup (Argon2 + ChaCha) is **always** available without this flag. |

---

## Permissions

Use `keyring-store:default` or granular `keyring-store:allow-*` (see [`permissions/default.toml`](permissions/default.toml)). Commands: `plugin:keyring-store|<command>`.

---

## Relationship to `tauri-plugin-stronghold`

| Stronghold | This plugin |
|------------|-------------|
| Password-derived snapshot | No snapshot file; OS stores secrets |
| `save()` writes snapshot | `save()` is a **no-op** (compat) |
| Procedures in Stronghold VM | In-process crypto; outputs in keyring |

---

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --no-default-features

pnpm install
pnpm build
pnpm test
```

Rustdoc logo (after push to `main`): PNG is generated from [`assets/docs-logo.svg`](assets/docs-logo.svg):

```bash
rsvg-convert -w 128 -h 128 assets/docs-logo.svg -o assets/docs-logo.png
```

---

## Testing

- **Rust**: `cargo test` — deterministic account-key tests, `map_keyring_err` unit tests, and serde roundtrips do not need D-Bus. Tests that call the real OS store are `#[ignore]`; run locally where Secret Service / Keychain is available.
- **JavaScript**: `pnpm test` (Vitest) mocks `@tauri-apps/api/core`.

---

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/s00d/tauri-plugin-keyring-store).

---

## Partners

Contributions and sponsorship help maintain this and related plugins. Thank you for your support.

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

`SPDX-License-Identifier: MIT OR Apache-2.0`
