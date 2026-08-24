# Dogfooding Problem Map — ygg Mobile Companion

Status: 2026-08-23, branch `feat/ygg-serve-mobile-companion`, single commit `172c284` (amended; was `d537fa6`) on main (`885a6ac`).
This is a local working document (untracked); it is not part of the feature commit.

## Scope snapshot

- Mobile app: `apps/mobile/src-tauri/` — thin Tauri iOS shell; no Tauri IPC capabilities granted to the webview; all logic runs on the host.
- Host side: `extensions/ygg-serve/src/companion.rs` (~3500 lines) + `extensions/ygg-companion-protocol/` (shared framing crate, 10 tests).
- Branch consolidated from stash `ebb1505` (backup ref `backup/mobile-companion-stash`, original `stash@{0}` both still intact).
- Targets ygg `0.6.0-dev` with Extensions API 0.2.

## Problems

### P1 — Committed serve web bundle lacked the pairing UI — RESOLVED
`extensions/ygg-serve/web/assets/app.js` as committed on main contained no companion code (0 matches). Dogfooding requires the pairing panel. Regenerated via `npm run sync:web` from merged source; the regenerated bundle contains the companion pairing UI (71 `companion` matches, 35 `pairing`, 2 `pairing ticket`) and folds the merge into `d537fa6`.
**Residual risk:** any future rebase/conflict that touches the bundle can silently repeat this. Precedence rule for bundle conflicts should be "regenerate from merged source", not "take main's".

### P2 — iOS device build & install requires explicit action — RESOLVED (build+install done)
Everything verified: Xcode 27 Beta 5, PATH fix in the generated project (see P7), aarch64-apple-ios compiles, signing cert for achuthanmukundan00@gmail.com with Team 43N9GMD9FX, device paired and available. **Done 2026-08-23:** `xcodebuild build` succeeded, app installed via `devicectl device install app` (bundle `org.skaft.ygg.companion`) and launched successfully on Achu's iPhone. Physical device did not need the simulator runtime.
**Remaining (user, manual):** pairing flow — open the serve launch link in a browser on the Mac → Connected Devices → Pair → paste ticket into the app on the phone.

### P3 — n0 relay requires network access — OPEN, environmental
`--companion-relay n0` uses iroh's public n0 relay (hardcoded production relay map in `extensions/ygg-serve/src/companion.rs:1897`, `RELAY_ONLINE_TIMEOUT` 20s). Without outbound network access to n0's relays, companion startup fails with `RelayUnavailable`. Note the relay is only a NAT-traversal/hole-punching assist (relay map + signaling), not a data intermediary; direct connections are preferred once established. Expect first-pairing latency while the relay map comes online.
Unrelated: `~/relay-remote-tui` and `~/relay-docs-edit` are the user's separate "Relay" project (LLM protocol adapter, github.com/achuthanmukundan00/relay) — not connected to this flag.

### P4 — Stale safety artifacts — OPEN, pending dogfood confirmation
`stash@{0}` (and `stash@{1}`, an older partial backup) plus `backup/mobile-companion-stash` → `ebb1505` remain. Cleanup (`git stash drop`, `git update-ref -d backup/mobile-companion-stash`) only after the user confirms the app pairs and works on the iPhone.

### P5 — Serve lockfile pinning risk — OPEN, latent (now also fixed in the ROOT lock)
`extensions/ygg-serve/Cargo.lock` carries stash-era pins (`pkcs8 0.11.0-rc.7`) required by `ed25519-dalek 3.0.0-pre.1`. Any future `cargo update` in the serve workspace can bump `pkcs8` to 0.11.0 and break the build again. Mitigation: avoid `cargo update` in that workspace, or pin `pkcs8` / add a `cargo update --precise` step; consider upstreaming a fix so the lock doesn't depend on pre-release pins.
**2026-08-23:** the ROOT `Cargo.lock` had the same drift (re-resolved during the rebase: `ed25519 3.0.0` → pulled stable `pkcs8 0.11.0`/`der 0.8.1`, which breaks `ed25519-dalek 3.0.0-pre.1` and `pkcs8 0.11.0-rc.7`). Fixed in `172c284` by pinning the whole RustCrypto pre-release family to the stash-era set: `ed25519 3.0.0-rc.0`, `pkcs8 0.11.0-rc.7`, `der 0.8.0-rc.9`, `spki 0.8.0-rc.4`. Same latent risk applies: `cargo update` in the root workspace will re-break the build until iroh ships a stable-compatible ed25519-dalek.

### P6 — Minor warnings — OPEN, cosmetic
`ygg-mobile` lib emits 1 warning (missing doc comment on the `tauri::mobile_entry_point` fn). No action required for dogfooding.

### P7 — Generated Xcode project is not reproducible from the repo — OPEN, accepted risk
`apps/mobile/src-tauri/gen/` is git-ignored (`.gitignore:7`). The required PATH fix (prepend `/opt/homebrew/bin:$HOME/.cargo/bin` to the "Build Rust Code" script phase, `project.pbxproj:259`) exists only in the local generated project. If the project is regenerated (`tauri ios init` etc.), the fix is lost and Xcode builds fail again with `npm: command not found`. Long-term fix: wrap tool resolution in a build script that Xcode's sanitized PATH can find, or commit a patch/spec for the generated project. Same applies to any icon replacement in `gen/apple/Assets.xcassets` (see P8).

### P8 — Icon not yet integrated — OPEN, needs Icon Composer GUI
Layered assets ready at `apps/mobile/icon-composer/` (`background.svg` full-bleed gradient + `tree.svg` braille points) for Icon Composer 2 Liquid Glass treatment; `preview.svg.png` verified visually. Remaining: import into Icon Composer 2, export `.icon`, replace AppIcon in `gen/apple/Assets.xcassets`. Output also lives in the ignored `gen/` tree → same regeneration risk as P7.
Note: Icon Composer ships `ictool`/`icrtool` CLIs, but `ictool` only exports images FROM `.icon` documents; authoring the `.icon` package appears GUI-only, so this step is manual.

### P9 — Stale installed ygg-serve application package — RESOLVED 2026-08-23
The package under `~/.ygg/extensions/ygg-serve/` was built Aug 20 from pre-companion code and declared `network = "loopback"`, so the stock launcher refused to dispatch (`must declare network='loopback+explicit-n0-relay'`). Rebuilt the runtime from the branch (debug profile, serve feature), hand-assembled the two-file archive (`ygg-serve/bin/ygg-serve-runtime` + `package.toml` with the correct capability string and sha256 — replicating `scripts/package-ygg-serve-release.sh`'s layout, which requires a clean tree + release binary), and installed via `ygg extension update --path`. Dispatch verified: the launcher exec-replaces into `ygg-serve-runtime serve --companion --companion-relay n0`.
Caveat: this local package is a debug build (204MB); a real release package still needs `cargo build --release --locked --target aarch64-apple-darwin -p ygg-coding-agent --features serve` + the packaging script (that release build OOM-killed at default parallelism on 16GB — retry with `-j 4` or similar).

### P10 — Feature-gated serve code was never compiled by the test suite — two latent compile errors, FIXED 2026-08-23
The rebase-era test runs did not enable the `serve` feature, so `crates/ygg-coding-agent/src/extensions/serve.rs` had two errors from main's newer `AgentEvent` shape: `ToolFinished` pattern missing the new `duration` field (E0027) and the non-exhaustive match missing `TurnStarted` (E0004). Fixed in `172c284` (`..` in the pattern — duration is already derived from parsed bash output/timestamps; no-op arm for advisory `TurnStarted`). Lesson: CI must also run `cargo check --features serve` (the mobile-quality job presumably doesn't).

### P11 — One-time launch links are consumed by any fetch
The `Open ygg once` link is single-use; even a curl without following redirects burns it (303 marks it used). Verification probes must keep the session cookie (`curl -c/-b`) or restart serve to mint a fresh link.

## Non-problems (verified during consolidation)

- Toolchain skew (Node 26 vs pinned 22, Rust 1.97 vs MSRV 1.86) verified compatible.
- Breathing composer commit dropped cleanly; main's `compiled_default_composer_border_is_static_during_work` passes; mobile `.gitignore` lines preserved in `d537fa6`.
- Test suites green at `d537fa6`: root 1691 passed, serve 384, protocol 10, web units 277.
