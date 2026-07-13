# ygg-coding-agent — Gate 0 + Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the `ygg` binary and reach the first vertical slice — launch → submit a prompt → stream text/reasoning → execute tools with live progress → persist the session → exit cleanly — after first passing the Gate-0 TUI integration spike.

**Architecture:** A procedural product shell over the frozen `ygg_agent::Agent`. A mode-agnostic `App` (model/session state) plus an `InteractiveShell` (all terminal I/O). The interactive experience is a single-threaded `tokio::select!` loop on the main thread with explicit idle/active phases, rendering through `sexy-tui-rs` (a `!Send` retained widget tree) driven by a custom `YggTerminal` + `crossterm::EventStream` (never `ProcessTerminal::start`). See the design spec: `docs/superpowers/specs/2026-07-12-ygg-coding-agent-design.md`.

**Tech Stack:** Rust 2021, tokio (current-thread), crossterm (event-stream), clap (derive), sexy-tui-rs (rust-port branch), anyhow, serde/toml, dirs; frozen `ygg-ai` + `ygg-agent`.

## Global Constraints

- Rust edition **2021**, `rust-version = 1.80` (inherit `[workspace.package]`).
- **Do not modify** `crates/ygg-ai` or `crates/ygg-agent`. Use their public APIs only. The reasoning-config fix (`AgentConfig.reasoning`) is already merged.
- Interactive runtime is **`#[tokio::main(flavor = "current_thread")]`**. Never call `sexy_tui::ProcessTerminal::start()`.
- Control sends are **queued**, never awaited inside the input arm of the active loop (a full control channel would stall `run.next()`).
- Compaction, resume, resources, themes are **out of scope for this plan** (Slices 2–6). This plan reaches the vertical slice only.
- TDD for all pure-logic units. Frequent commits (one per task minimum). DRY. YAGNI.
- Binary crate: add `#![allow(missing_docs)]` at crate root (the workspace sets `missing_docs = "warn"`; a bin has no meaningful public API).
- Every task ends green: `cargo build -p ygg-coding-agent` and any task tests pass; workspace stays green (`cargo test --workspace` unaffected).

---

## File Structure (this plan)

- `Cargo.toml` (workspace) — add `crates/ygg-coding-agent` member.
- `crates/ygg-coding-agent/Cargo.toml` — crate manifest + deps.
- `crates/ygg-coding-agent/src/main.rs` — entry, `#[tokio::main(current_thread)]`, dispatch.
- `crates/ygg-coding-agent/src/cli.rs` — clap `Cli`; `Cli → Config` overrides.
- `crates/ygg-coding-agent/src/config.rs` — `Config`, workspace resolution, `resolve()`.
- `crates/ygg-coding-agent/src/session_store.rs` — `SessionStore`, path layout, `latest`.
- `crates/ygg-coding-agent/src/app/mod.rs` — `App` struct.
- `crates/ygg-coding-agent/src/app/bootstrap.rs` — `Bootstrap`, `LaunchSelection`, `resolve_launch_*`, `build_app`, tool-schema estimate.
- `crates/ygg-coding-agent/src/tui/mod.rs` — module glue.
- `crates/ygg-coding-agent/src/tui/terminal.rs` — `YggTerminal`, `force_restore`, `install_panic_hook`.
- `crates/ygg-coding-agent/src/tui/keymap.rs` — `InputAction`, `translate_active` (pure).
- `crates/ygg-coding-agent/src/tui/view.rs` — `InteractiveShell` (minimal for Slice 1).
- `crates/ygg-coding-agent/src/modes/mod.rs`, `interactive.rs`, `print.rs` — run modes.
- `crates/ygg-coding-agent/src/spike/bin_spike.rs` — Gate-0 spike ([[bin]]).
- External: a `sexy-tui-rs` rust-port git worktree at a stable path (Task 1).

---

## Task 1: Crate scaffold, sexy-tui-rs dependency, buildable binary

**Files:**
- Modify: `Cargo.toml` (workspace `members`)
- Create: `crates/ygg-coding-agent/Cargo.toml`
- Create: `crates/ygg-coding-agent/src/main.rs`

**Interfaces:**
- Produces: a compiling `ygg` binary that prints nothing useful yet; the workspace builds sexy-tui-rs as a path dependency.

- [ ] **Step 1: Materialize the sexy-tui-rs rust-port worktree at a stable path**

Run:
```bash
git -C ~/github/achuthanmukundan00/sexy-tui-rs worktree add -f \
  ~/github/achuthanmukundan00/sexy-tui-rs-rust-port rust-port
ls ~/github/achuthanmukundan00/sexy-tui-rs-rust-port/src/lib.rs
```
Expected: the path prints (worktree checked out on `rust-port`).

- [ ] **Step 2: Verify sexy-tui-rs builds on its own**

Run:
```bash
cargo build --manifest-path ~/github/achuthanmukundan00/sexy-tui-rs-rust-port/Cargo.toml
```
Expected: `Finished` (a clean library build). If it fails, STOP — Gate 0 cannot proceed without a buildable TUI dependency; report the errors.

- [ ] **Step 3: Add the workspace member**

Edit `Cargo.toml` (workspace root):
```toml
[workspace]
resolver = "2"
members = ["crates/ygg-ai", "crates/ygg-agent", "crates/ygg-coding-agent"]
```

- [ ] **Step 4: Create the crate manifest**

Create `crates/ygg-coding-agent/Cargo.toml`:
```toml
[package]
name = "ygg-coding-agent"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Local-first Rust coding agent over ygg-agent."

[lints]
workspace = true

[[bin]]
name = "ygg"
path = "src/main.rs"

[dependencies]
ygg-ai = { path = "../ygg-ai" }
ygg-agent = { path = "../ygg-agent" }
sexy-tui-rs = { path = "../../../../achuthanmukundan00/sexy-tui-rs-rust-port" }
tokio = { version = "1", features = ["rt", "macros", "time", "io-util", "process"] }
crossterm = { version = "0.28", features = ["event-stream"] }
futures-util = "0.3"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
toml = "0.8"
dirs = "5"
anyhow = "1"
```
> Note: the `sexy-tui-rs` path is relative to `crates/ygg-coding-agent/`. Adjust the `../` depth if your checkout differs; verify with Step 6.

- [ ] **Step 5: Create a minimal buildable main**

Create `crates/ygg-coding-agent/src/main.rs`:
```rust
#![allow(missing_docs)]

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("ygg: coding agent (scaffold)");
    Ok(())
}
```

- [ ] **Step 6: Build the whole workspace**

Run:
```bash
cargo build -p ygg-coding-agent
```
Expected: `Finished`. If the sexy-tui path is wrong, cargo reports "failed to read … Cargo.toml"; fix the `path` and rebuild.

- [ ] **Step 7: Confirm the frozen crates are untouched and green**

Run:
```bash
cargo test -p ygg-ai -p ygg-agent 2>&1 | tail -5
```
Expected: all pass (unchanged).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/ygg-coding-agent/Cargo.toml crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): scaffold ygg binary crate + sexy-tui-rs dep"
```

---

## Task 2: Workspace resolution

**Files:**
- Create: `crates/ygg-coding-agent/src/config.rs`
- Modify: `crates/ygg-coding-agent/src/main.rs` (add `mod config;`)
- Test: inline `#[cfg(test)]` in `config.rs`

**Interfaces:**
- Produces: `pub fn resolve_workspace(explicit: Option<&Path>, cwd: &Path) -> std::io::Result<PathBuf>` — `explicit` if given, else nearest ancestor of `cwd` containing `.git`, else `cwd`; result is canonicalized.

- [ ] **Step 1: Write the failing test**

In `crates/ygg-coding-agent/src/config.rs`:
```rust
#![allow(missing_docs)]
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn explicit_workspace_wins_and_is_canonicalized() {
        let dir = tempdir();
        let ws = resolve_workspace(Some(dir.path()), Path::new("/")).unwrap();
        assert_eq!(ws, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn finds_nearest_git_ancestor() {
        let dir = tempdir();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let ws = resolve_workspace(None, &nested).unwrap();
        assert_eq!(ws, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn falls_back_to_cwd_without_git() {
        let dir = tempdir();
        let ws = resolve_workspace(None, dir.path()).unwrap();
        assert_eq!(ws, dir.path().canonicalize().unwrap());
    }

    fn tempdir() -> tempfile::TempDir { tempfile::tempdir().unwrap() }
}
```
Add `tempfile = "3"` under `[dev-dependencies]` in the crate manifest.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent resolve_workspace 2>&1 | tail -20`
Expected: FAIL — `cannot find function resolve_workspace`.

- [ ] **Step 3: Write the minimal implementation**

Above the `#[cfg(test)]` block in `config.rs`:
```rust
/// Resolve the workspace root: `explicit` if given, else the nearest ancestor of
/// `cwd` containing `.git`, else `cwd`. The result is canonicalized.
pub fn resolve_workspace(explicit: Option<&Path>, cwd: &Path) -> std::io::Result<PathBuf> {
    if let Some(p) = explicit {
        return p.canonicalize();
    }
    let mut cur: Option<&Path> = Some(cwd);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return dir.canonicalize();
        }
        cur = dir.parent();
    }
    cwd.canonicalize()
}
```

Add to `main.rs`: `mod config;`

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ygg-coding-agent resolve_workspace 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/config.rs crates/ygg-coding-agent/src/main.rs crates/ygg-coding-agent/Cargo.toml
git commit -m "feat(coding-agent): workspace resolution (--workspace | .git ancestor | cwd)"
```

---

## Task 3: CLI parsing → Config

**Files:**
- Create: `crates/ygg-coding-agent/src/cli.rs`
- Modify: `crates/ygg-coding-agent/src/config.rs` (add `Config`, `Mode`, `ResumeSelector`, `SandboxPolicy`, `CompactionPolicy`, `resolve`)
- Modify: `crates/ygg-coding-agent/src/main.rs` (`mod cli;`)
- Test: inline in `cli.rs`

**Interfaces:**
- Consumes: `config::resolve_workspace` (Task 2).
- Produces:
  - `cli::Cli` (clap derive) with fields `prompt: Option<String>`, `print: bool`, `continue_: bool` (`--continue`), `resume: Option<Option<String>>`, `model: Option<String>`, `reasoning: Option<String>`, `workspace: Option<PathBuf>`, `show_reasoning: bool`.
  - `pub fn cli::build_config(cli: Cli, cwd: &Path) -> anyhow::Result<Config>`.
  - `config::Config` with fields per the spec §5.1 (subset for Slice 1): `workspace`, `invocation_cwd`, `model: Option<ModelId>`, `reasoning: ReasoningConfig`, `sandbox: SandboxPolicy`, `session_dir`, `max_turns: u64`, `show_reasoning_in_print: bool`, `mode: Mode`, `resume: ResumeSelector`.
  - `config::Mode::{Interactive, Print{prompt}}`, `config::ResumeSelector::{New, Continue, Resume(Option<String>)}`.

- [ ] **Step 1: Write the failing test**

Create `crates/ygg-coding-agent/src/cli.rs`:
```rust
#![allow(missing_docs)]
use std::path::{Path, PathBuf};
use clap::Parser;
use crate::config::{Config, Mode, ResumeSelector};

/// `ygg` command line.
#[derive(Parser, Debug)]
#[command(name = "ygg", about = "Local-first coding agent")]
pub struct Cli {
    /// Optional first prompt (seeds a new session; required text in --print).
    pub prompt: Option<String>,
    /// Non-interactive print mode.
    #[arg(long, short = 'p')]
    pub print: bool,
    /// Continue the most recent session for this workspace.
    #[arg(long = "continue")]
    pub continue_: bool,
    /// Resume a session (optionally by id).
    #[arg(long, value_name = "ID")]
    pub resume: Option<Option<String>>,
    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning: off | minimal | low | medium | high | budget=<n>.
    #[arg(long)]
    pub reasoning: Option<String>,
    /// Workspace root override.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// In print mode, also emit reasoning.
    #[arg(long)]
    pub show_reasoning: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

    #[test]
    fn print_mode_requires_prompt_text() {
        let dir = cwd();
        let cli = Cli { prompt: None, print: true, continue_: false, resume: None,
            model: Some("m".into()), reasoning: None, workspace: Some(dir.path().into()),
            show_reasoning: false };
        assert!(build_config(cli, dir.path()).is_err());
    }

    #[test]
    fn print_mode_builds_print_config() {
        let dir = cwd();
        let cli = Cli { prompt: Some("hi".into()), print: true, continue_: false, resume: None,
            model: Some("m".into()), reasoning: None, workspace: Some(dir.path().into()),
            show_reasoning: true };
        let cfg = build_config(cli, dir.path()).unwrap();
        assert!(matches!(cfg.mode, Mode::Print { .. }));
        assert!(cfg.show_reasoning_in_print);
    }

    #[test]
    fn continue_sets_resume_selector() {
        let dir = cwd();
        let cli = Cli { prompt: None, print: false, continue_: true, resume: None,
            model: None, reasoning: None, workspace: Some(dir.path().into()), show_reasoning: false };
        let cfg = build_config(cli, dir.path()).unwrap();
        assert!(matches!(cfg.resume, ResumeSelector::Continue));
        assert!(matches!(cfg.mode, Mode::Interactive));
    }

    #[test]
    fn reasoning_parses_budget_and_effort() {
        let dir = cwd();
        let mk = |r: &str| Cli { prompt: None, print: false, continue_: false, resume: None,
            model: None, reasoning: Some(r.into()), workspace: Some(dir.path().into()), show_reasoning: false };
        assert!(build_config(mk("off"), dir.path()).is_ok());
        assert!(build_config(mk("high"), dir.path()).is_ok());
        assert!(build_config(mk("budget=2048"), dir.path()).is_ok());
        assert!(build_config(mk("nonsense"), dir.path()).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent --lib cli 2>&1 | tail -20`
Expected: FAIL — `Config`, `Mode`, `ResumeSelector`, `build_config` not found.

- [ ] **Step 3: Add the config types**

In `crates/ygg-coding-agent/src/config.rs`, above the tests:
```rust
use std::time::Duration;
use ygg_ai::{ModelId, ReasoningConfig, ReasoningEffort};
use ygg_agent::SandboxConfig;

pub enum Mode { Interactive, Print { prompt: String } }
pub enum ResumeSelector { New, Continue, Resume(Option<String>) }

pub struct SandboxPolicy {
    pub allow_edit: bool,
    pub allow_process: bool,
    pub allow_shell: bool,
    pub exec_timeout_secs: u64,
    pub max_output_bytes: usize,
}
impl Default for SandboxPolicy {
    fn default() -> Self {
        Self { allow_edit: true, allow_process: true, allow_shell: false,
                exec_timeout_secs: 120, max_output_bytes: 64 * 1024 }
    }
}
impl SandboxPolicy {
    pub fn to_sandbox_config(&self, workspace: &Path) -> SandboxConfig {
        let mut s = SandboxConfig::new(workspace);
        s.allow_edit = self.allow_edit;
        s.allow_process = self.allow_process;
        s.allow_shell = self.allow_shell;
        s.exec_timeout = Duration::from_secs(self.exec_timeout_secs);
        s.max_output_bytes = self.max_output_bytes;
        s
    }
}

pub struct Config {
    pub workspace: PathBuf,
    pub invocation_cwd: PathBuf,
    pub model: Option<ModelId>,
    pub reasoning: ReasoningConfig,
    pub sandbox: SandboxPolicy,
    pub session_dir: PathBuf,
    pub max_turns: u64,
    pub show_reasoning_in_print: bool,
    pub mode: Mode,
    pub resume: ResumeSelector,
}

/// Parse the reasoning override string into a canonical `ReasoningConfig`.
pub fn parse_reasoning(s: &str) -> anyhow::Result<ReasoningConfig> {
    Ok(match s {
        "off" => ReasoningConfig::Off,
        "minimal" => ReasoningConfig::Effort(ReasoningEffort::Minimal),
        "low" => ReasoningConfig::Effort(ReasoningEffort::Low),
        "medium" => ReasoningConfig::Effort(ReasoningEffort::Medium),
        "high" => ReasoningConfig::Effort(ReasoningEffort::High),
        other => {
            let n = other.strip_prefix("budget=")
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("invalid --reasoning {other:?}"))?;
            ReasoningConfig::Budget(n)
        }
    })
}

/// Default session directory: `~/.ygg/sessions`.
pub fn default_session_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".ygg").join("sessions")
}
```

- [ ] **Step 4: Implement `build_config` in `cli.rs`**

Below `Cli` (before the tests):
```rust
/// Build a resolved `Config` from parsed CLI args (this plan omits the config-file
/// layers, added in Slice 4).
pub fn build_config(cli: Cli, cwd: &Path) -> anyhow::Result<Config> {
    let workspace = crate::config::resolve_workspace(cli.workspace.as_deref(), cwd)?;
    let invocation_cwd = cwd.canonicalize()?;
    let reasoning = match &cli.reasoning {
        Some(r) => crate::config::parse_reasoning(r)?,
        None => ygg_ai::ReasoningConfig::Off,
    };
    let mode = if cli.print {
        let prompt = cli.prompt.clone()
            .ok_or_else(|| anyhow::anyhow!("--print requires a prompt: ygg --print \"...\""))?;
        Mode::Print { prompt }
    } else {
        Mode::Interactive
    };
    let resume = if cli.continue_ {
        ResumeSelector::Continue
    } else if let Some(id) = cli.resume {
        ResumeSelector::Resume(id)
    } else {
        ResumeSelector::New
    };
    Ok(Config {
        workspace,
        invocation_cwd,
        model: cli.model.map(ygg_ai::ModelId),
        reasoning,
        sandbox: crate::config::SandboxPolicy::default(),
        session_dir: crate::config::default_session_dir(),
        max_turns: 40,
        show_reasoning_in_print: cli.show_reasoning,
        mode,
        resume,
    })
}
```

Add to `main.rs`: `mod cli;`

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib cli 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/ygg-coding-agent/src/cli.rs crates/ygg-coding-agent/src/config.rs crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): CLI parsing and Config assembly"
```

---

## Task 4: SessionStore path layout

**Files:**
- Create: `crates/ygg-coding-agent/src/session_store.rs`
- Modify: `main.rs` (`mod session_store;`)
- Test: inline

**Interfaces:**
- Produces:
  - `SessionStore::new(session_dir: &Path, workspace: &Path) -> SessionStore` (computes a stable per-workspace subdir).
  - `SessionStore::dir(&self) -> &Path`.
  - `SessionStore::new_path(&self, now_stamp: &str) -> PathBuf` (filename `<stamp>-<rand4>.jsonl`; `now_stamp`/rand injected for testability).
  - `SessionStore::list(&self) -> Vec<SessionMeta>` (newest-first by mtime; title left empty in Slice 1).
  - `SessionStore::latest(&self) -> anyhow::Result<SessionMeta>`.
  - `pub struct SessionMeta { pub id: String, pub path: PathBuf, pub modified: SystemTime, pub title: String }`.

- [ ] **Step 1: Write the failing test**

Create `crates/ygg-coding-agent/src/session_store.rs`:
```rust
#![allow(missing_docs)]
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_workspace_dirs_are_stable_and_distinct() {
        let root = tempfile::tempdir().unwrap();
        let ws_a = tempfile::tempdir().unwrap();
        let ws_b = tempfile::tempdir().unwrap();
        let a1 = SessionStore::new(root.path(), ws_a.path());
        let a2 = SessionStore::new(root.path(), ws_a.path());
        let b1 = SessionStore::new(root.path(), ws_b.path());
        assert_eq!(a1.dir(), a2.dir(), "same workspace ⇒ same dir");
        assert_ne!(a1.dir(), b1.dir(), "different workspace ⇒ different dir");
        assert!(a1.dir().starts_with(root.path()));
    }

    #[test]
    fn new_path_is_inside_dir_with_jsonl_extension() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), ws.path());
        let p = store.new_path("2026-07-12T14-30-05Z");
        assert!(p.starts_with(store.dir()));
        assert_eq!(p.extension().unwrap(), "jsonl");
        assert!(p.file_name().unwrap().to_str().unwrap().starts_with("2026-07-12T14-30-05Z-"));
    }

    #[test]
    fn latest_returns_newest_by_mtime() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), ws.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.dir().join("2026-01-01T00-00-00Z-aaaa.jsonl"), b"").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(store.dir().join("2026-02-02T00-00-00Z-bbbb.jsonl"), b"").unwrap();
        let latest = store.latest().unwrap();
        assert_eq!(latest.id, "2026-02-02T00-00-00Z-bbbb");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent --lib session_store 2>&1 | tail -20`
Expected: FAIL — `SessionStore` not found.

- [ ] **Step 3: Write the minimal implementation**

Above the tests:
```rust
/// Filesystem session store, scoped per workspace.
pub struct SessionStore { dir: PathBuf }

pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub title: String,
}

fn workspace_key(workspace: &Path) -> String {
    // FNV-1a over the canonical path bytes → 12 hex chars. Dependency-free and stable.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in workspace.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:012x}")
}

impl SessionStore {
    pub fn new(session_dir: &Path, workspace: &Path) -> Self {
        Self { dir: session_dir.join(workspace_key(workspace)) }
    }
    pub fn dir(&self) -> &Path { &self.dir }

    /// `<stamp>-<rand4>.jsonl`. Caller supplies the timestamp string (testable).
    pub fn new_path(&self, stamp: &str) -> PathBuf {
        // Cheap non-crypto disambiguator from the OS thread id + stamp bytes.
        let mut h: u32 = 0x811c_9dc5;
        for b in stamp.as_bytes() { h ^= u32::from(*b); h = h.wrapping_mul(0x0100_0193); }
        self.dir.join(format!("{stamp}-{:04x}.jsonl", (h & 0xffff)))
    }

    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas: Vec<SessionMeta> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") { return None; }
                let id = path.file_stem()?.to_string_lossy().into_owned();
                let modified = e.metadata().ok()?.modified().ok()?;
                Some(SessionMeta { id, path, modified, title: String::new() })
            })
            .collect();
        metas.sort_by(|a, b| b.modified.cmp(&a.modified)); // newest first
        metas
    }

    pub fn latest(&self) -> anyhow::Result<SessionMeta> {
        self.list().into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("no sessions for this workspace yet"))
    }
}
```

Add to `main.rs`: `mod session_store;`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib session_store 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/session_store.rs crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): filesystem SessionStore (per-workspace layout, latest)"
```

---

## Task 5: Bootstrap, launch selection, and `build_app` (with frozen tool-schema reserve)

**Files:**
- Create: `crates/ygg-coding-agent/src/app/mod.rs`
- Create: `crates/ygg-coding-agent/src/app/bootstrap.rs`
- Modify: `main.rs` (`mod app;`)
- Test: inline in `bootstrap.rs`

**Interfaces:**
- Consumes: `Config`, `SessionStore`, `ResumeSelector`.
- Produces:
  - `app::App { agent, model, client, config, system_tokens, tool_schema_tokens }`.
  - `app::bootstrap::Bootstrap { config, catalog, sessions, client }`.
  - `app::bootstrap::LaunchSelection { model: ModelId, session: SessionSelection }`, `SessionSelection::{OpenExisting(PathBuf), CreateNew(PathBuf)}`.
  - `pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap>`.
  - `pub fn resolve_launch_print(boot: &Bootstrap, stamp: &str) -> anyhow::Result<LaunchSelection>`.
  - `pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App>`.
  - `pub fn estimate_text_tokens(s: &str) -> u64` (used here and by Slice 3).

- [ ] **Step 1: Write the failing test (print launch + tool-schema reserve)**

Create `crates/ygg-coding-agent/src/app/bootstrap.rs`:
```rust
#![allow(missing_docs)]
use std::path::PathBuf;
use ygg_ai::{AiClient, ModelCatalog, ModelId};
use crate::config::{Config, Mode, ResumeSelector};
use crate::session_store::SessionStore;

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config(model: Option<&str>, resume: ResumeSelector) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().canonicalize().unwrap();
        std::mem::forget(dir); // keep the temp dir for the test's lifetime
        Config {
            workspace: p.clone(), invocation_cwd: p.clone(),
            model: model.map(|m| ModelId(m.into())),
            reasoning: ygg_ai::ReasoningConfig::Off,
            sandbox: crate::config::SandboxPolicy::default(),
            session_dir: std::env::temp_dir().join(format!("ygg-test-{}", std::process::id())),
            max_turns: 40, show_reasoning_in_print: false,
            mode: Mode::Print { prompt: "hi".into() }, resume,
        }
    }

    #[test]
    fn print_launch_errors_without_model() {
        let boot = bootstrap(base_config(None, ResumeSelector::New)).unwrap();
        let err = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap_err();
        assert!(err.to_string().contains("no model configured"), "{err}");
    }

    #[test]
    fn print_launch_creates_new_session_path() {
        let boot = bootstrap(base_config(Some("gpt-4o-mini"), ResumeSelector::New)).unwrap();
        let launch = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap();
        assert!(matches!(launch.session, SessionSelection::CreateNew(_)));
        assert_eq!(launch.model.0, "gpt-4o-mini");
    }

    #[test]
    fn tool_schema_reserve_reads_frozen_definitions() {
        // The four CoreTools defs must contribute a positive, deterministic reserve.
        let t = tool_schema_reserve();
        assert!(t > 0);
        assert_eq!(t, tool_schema_reserve(), "deterministic");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent --lib bootstrap 2>&1 | tail -20`
Expected: FAIL — `bootstrap`, `resolve_launch_print`, `SessionSelection`, `tool_schema_reserve` not found.

- [ ] **Step 3: Write `App`**

Create `crates/ygg-coding-agent/src/app/mod.rs`:
```rust
#![allow(missing_docs)]
pub mod bootstrap;

use ygg_agent::Agent;
use ygg_ai::{AiClient, Model};
use crate::config::Config;

/// Mode-agnostic owning struct. No Theme, no TUI.
pub struct App {
    pub agent: Agent,
    pub model: Model,
    pub client: AiClient,
    pub config: Config,
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
}
```

- [ ] **Step 4: Write the bootstrap implementation**

In `bootstrap.rs`, above the tests:
```rust
use ygg_ai::ToolDef;
use ygg_agent::{Agent, AgentConfig, CoreTools, ExtensionHost, Session, Tool,
                ReadTool, SearchTool, EditTool, ExecTool};
use crate::app::App;

pub struct Bootstrap {
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub client: AiClient,
}

pub enum SessionSelection { OpenExisting(PathBuf), CreateNew(PathBuf) }

pub struct LaunchSelection {
    pub model: ModelId,
    pub session: SessionSelection,
}

pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap> {
    let catalog = ModelCatalog::builtin()?;
    let sessions = SessionStore::new(&config.session_dir, &config.workspace);
    let client = AiClient::new();
    Ok(Bootstrap { config, catalog, sessions, client })
}

/// Print resolution NEVER opens a picker; unresolved model ⇒ actionable error.
pub fn resolve_launch_print(boot: &Bootstrap, stamp: &str) -> anyhow::Result<LaunchSelection> {
    let model = boot.config.model.clone().ok_or_else(|| anyhow::anyhow!(
        "no model configured: pass --model <id> (available: {})",
        boot.catalog.models().map(|m| m.id.0.clone()).collect::<Vec<_>>().join(", ")
    ))?;
    let session = match &boot.config.resume {
        ResumeSelector::New              => SessionSelection::CreateNew(boot.sessions.new_path(stamp)),
        ResumeSelector::Continue         => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => SessionSelection::OpenExisting(boot.sessions.dir().join(format!("{id}.jsonl"))),
        ResumeSelector::Resume(None)     => anyhow::bail!("--resume needs a session id in print mode"),
    };
    Ok(LaunchSelection { model, session })
}

/// Conservative ~4-bytes-per-token estimate.
pub fn estimate_text_tokens(s: &str) -> u64 { (s.len() as u64 + 3) / 4 }

fn estimate_tooldef_tokens(def: &ToolDef) -> u64 {
    let serialized = serde_json::to_string(def).map(|s| s.len()).unwrap_or(0);
    (serialized as u64 + 3) / 4
}

/// Tool-schema reserve read from the FROZEN source of truth — the exact tools
/// `CoreTools` registers, via the public `Tool::definition()`. No JSON reproduced.
pub fn tool_schema_reserve() -> u64 {
    [ReadTool.definition(), SearchTool.definition(), EditTool.definition(), ExecTool.definition()]
        .iter().map(estimate_tooldef_tokens).sum()
}

pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App> {
    let Bootstrap { config, catalog, client, .. } = boot;
    let model = catalog.resolve(&launch.model)?;

    let session = match launch.session {
        SessionSelection::CreateNew(p) => {
            if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
            Session::create(p)?
        }
        SessionSelection::OpenExisting(p) => Session::open(p)?,
    };

    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);

    let system_tokens = estimate_text_tokens(&system);
    let tool_schema_tokens = tool_schema_reserve();

    let agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system,
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: config.reasoning.clone(),
    })?;
    Ok(App { agent, model, client, config, system_tokens, tool_schema_tokens })
}
```

Add to `main.rs`: `mod app;`

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib bootstrap 2>&1 | tail -20`
Expected: PASS (3 tests). (`gpt-4o-mini` is in the builtin catalog, `catalog.json`.)

- [ ] **Step 6: Commit**

```bash
git add crates/ygg-coding-agent/src/app crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): bootstrap, print launch selection, build_app + frozen tool reserve"
```

---

## Task 6: Gate 0 — TUI integration spike (HARD ARCHITECTURE GATE)

> **This is a manual, run-it-yourself gate.** It cannot be unit-tested (it needs a real TTY). If any pass criterion fails, STOP and report — the interactive architecture is invalidated and must change before Slice 1 continues (fallback: dedicated blocking-input thread bridged by a channel, or a non-blocking sexy-tui driver).

**Files:**
- Create: `crates/ygg-coding-agent/src/tui/mod.rs`
- Create: `crates/ygg-coding-agent/src/tui/terminal.rs`
- Create: `crates/ygg-coding-agent/src/spike/bin_spike.rs`
- Modify: `crates/ygg-coding-agent/Cargo.toml` (add the spike `[[bin]]`)

**Interfaces:**
- Produces: `tui::terminal::{YggTerminal, force_restore, install_panic_hook}`.
  - `YggTerminal::enter() -> anyhow::Result<YggTerminal>`; `impl sexy_tui::Terminal`; `set_size(&mut self, u16, u16)`.
  - `force_restore()` idempotent via a global atomic; `install_panic_hook()` chains the prior hook.

- [ ] **Step 1: Implement `YggTerminal`, `force_restore`, `install_panic_hook`, and a unit test for idempotent restore**

Create `crates/ygg-coding-agent/src/tui/mod.rs`:
```rust
#![allow(missing_docs)]
pub mod terminal;
```

Create `crates/ygg-coding-agent/src/tui/terminal.rs`:
```rust
#![allow(missing_docs)]
use std::io::{Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use crossterm::{cursor, execute, terminal};

static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Restore the terminal at most once (idempotent). Safe from Drop and the panic hook.
pub fn force_restore() {
    if RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = out.flush();
    }
}

/// Install once at startup; chains the previous panic hook.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { force_restore(); prev(info); }));
}

pub struct YggTerminal { out: Stdout, cols: u16, rows: u16 }

impl YggTerminal {
    pub fn enter() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        Ok(Self { out, cols, rows })
    }
    pub fn set_size(&mut self, cols: u16, rows: u16) { self.cols = cols; self.rows = rows; }
}

impl Drop for YggTerminal { fn drop(&mut self) { force_restore(); } }

impl sexy_tui::Terminal for YggTerminal {
    fn start(&mut self, _on_input: Box<dyn FnMut(&str)>, _on_resize: Box<dyn FnMut()>) {
        unreachable!("YggTerminal::start is never called; input is driven by the select! loop");
    }
    fn write(&mut self, data: &str) { let _ = self.out.write_all(data.as_bytes()); let _ = self.out.flush(); }
    fn columns(&self) -> u16 { self.cols }
    fn rows(&self) -> u16 { self.rows }
    fn move_by(&mut self, lines: i16) {
        let _ = if lines > 0 { execute!(self.out, cursor::MoveDown(lines as u16)) }
                else if lines < 0 { execute!(self.out, cursor::MoveUp((-lines) as u16)) }
                else { Ok(()) };
    }
    fn hide_cursor(&mut self) { let _ = execute!(self.out, cursor::Hide); }
    fn show_cursor(&mut self) { let _ = execute!(self.out, cursor::Show); }
    fn clear_line(&mut self) { let _ = execute!(self.out, terminal::Clear(terminal::ClearType::CurrentLine)); }
    fn clear_from_cursor(&mut self) { let _ = execute!(self.out, terminal::Clear(terminal::ClearType::FromCursorDown)); }
    fn clear_screen(&mut self) { let _ = execute!(self.out, terminal::Clear(terminal::ClearType::All)); }
    fn enable_mouse_capture(&mut self) {}
    fn disable_mouse_capture(&mut self) {}
    fn enter_alternate_screen(&mut self) { let _ = execute!(self.out, terminal::EnterAlternateScreen); }
    fn leave_alternate_screen(&mut self) { let _ = execute!(self.out, terminal::LeaveAlternateScreen); }
    fn set_title(&mut self, title: &str) { let _ = execute!(self.out, terminal::SetTitle(title)); }
    fn set_progress(&mut self, _active: bool) {}
    fn drain_input(&mut self, _max_ms: u64, _idle_ms: u64) {}
    fn stop(&mut self) { force_restore(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn force_restore_is_idempotent_without_a_terminal() {
        // With RAW_ACTIVE false, force_restore must be a no-op and never panic.
        RAW_ACTIVE.store(false, Ordering::SeqCst);
        force_restore();
        force_restore();
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
    }
}
```

> Verify the `Terminal` trait method set matches `sexy-tui-rs` exactly (see `terminal.rs` in the rust-port). If the trait differs, adjust the impl; a missing/extra method is a compile error caught in Step 4.

- [ ] **Step 2: Write the spike binary**

Create `crates/ygg-coding-agent/src/spike/bin_spike.rs`:
```rust
#![allow(missing_docs)]
use std::time::Duration;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use sexy_tui::widgets::{Markdown, MarkdownOptions, MarkdownTheme};

#[path = "../tui/terminal.rs"]
mod terminal;
use terminal::{YggTerminal, force_restore, install_panic_hook};

fn markdown(text: &str) -> Box<Markdown> {
    Box::new(Markdown::new(text, MarkdownTheme::default(), MarkdownOptions::default()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    install_panic_hook();
    let term = YggTerminal::enter()?;
    let mut tui = sexy_tui::TUI::new(Box::new(term));
    tui.add_child(markdown("# spike\ntype; `q` or Ctrl+C quits; resize the window."));
    tui.start(); // MUST NOT reach YggTerminal::start (unreachable!()) — proves it does not call Terminal::start
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut buf = String::from("# spike\n");
    loop {
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(Event::Key(k)))
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) => break,
                Some(Ok(Event::Key(k))) if k.code == KeyCode::Char('q') => break,
                Some(Ok(Event::Key(k))) => {
                    if let KeyCode::Char(c) = k.code {
                        buf.push(c);
                        tui.children_mut().clear();
                        tui.add_child(markdown(&buf));
                        tui.request_render();
                    }
                }
                Some(Ok(Event::Resize(_, _))) => tui.request_render(),
                Some(Err(_)) | None => break,
                _ => {}
            },
            _ = ticker.tick() => tui.request_render(),
        }
    }
    tui.stop();
    let _ = force_restore();
    Ok(())
}
```

> `MarkdownTheme`/`MarkdownOptions` may not implement `Default` — if Step 4 errors, construct them explicitly from the rust-port's field set (check `widgets/markdown.rs`). `children_mut()` + `clear()` is how the spike swaps content (confirmed in `tui.rs`).

- [ ] **Step 3: Register the spike binary**

Add to `crates/ygg-coding-agent/Cargo.toml`:
```toml
[[bin]]
name = "ygg-tui-spike"
path = "src/spike/bin_spike.rs"
```

- [ ] **Step 4: Build the spike**

Run: `cargo build -p ygg-coding-agent --bin ygg-tui-spike 2>&1 | tail -20`
Expected: `Finished`. Fix any `Terminal`/widget API mismatches until it builds.

- [ ] **Step 5: Run the terminal-restore unit test**

Run: `cargo test -p ygg-coding-agent --lib terminal 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: MANUAL GATE — run the spike in a real terminal and verify all criteria**

Run (in an interactive terminal, not through a pipe): `cargo run -p ygg-coding-agent --bin ygg-tui-spike`

Verify **all**:
1. It renders and updates the Markdown as you type — with no call to `ProcessTerminal::start()`.
2. It ran past `tui.start()` without panicking — proving `TUI::start()` does not call `Terminal::start()` (which is `unreachable!()`).
3. It keeps repainting on the 80 ms ticker even with no input (proves paint on a non-input async wakeup — the mechanism agent events will use).
4. Resizing the window reflows without corruption.
5. `q`, **Ctrl+C (handled as a raw key event, not a signal)**, and a forced panic all restore the terminal: after exit the shell prompt is normal (cursor visible, no raw mode, primary screen).
6. It ran under `flavor = "current_thread"` (no `Send` errors surfaced at build).

- [ ] **Step 7: Record the gate result**

If all six pass, note it in the commit message and proceed. If any fail, STOP and report which criterion failed and the observed behavior.

- [ ] **Step 8: Commit**

```bash
git add crates/ygg-coding-agent/src/tui crates/ygg-coding-agent/src/spike crates/ygg-coding-agent/Cargo.toml
git commit -m "feat(coding-agent): Gate 0 TUI spike + YggTerminal (idempotent restore) — gate PASSED"
```

---

## Task 7: Keymap translation (pure)

**Files:**
- Create: `crates/ygg-coding-agent/src/tui/keymap.rs`
- Modify: `tui/mod.rs` (`pub mod keymap;`)
- Test: inline

**Interfaces:**
- Produces:
  - `pub enum InputAction { Abort, Steer(String), FollowUp(String), Submit(String), Edit(EditAction), Resize(u16,u16), Closed, Ignore }`.
  - `pub enum EditAction { Char(char), Backspace, Newline }`.
  - `pub fn translate(maybe: Option<Result<crossterm::event::Event, std::io::Error>>, active: bool, pending_is_empty: bool) -> InputAction`.

> Slice 1 keymap: `Ctrl+C` → `Abort` (active) / `Closed` (idle); `Enter` → `Submit`/`FollowUp` (active); `Shift+Enter` handling deferred (use `Alt+Enter` → newline for now); printable chars → `Edit(Char)`; `Backspace` → `Edit(Backspace)`; `Resize` → `Resize`; stream end → `Closed`.

- [ ] **Step 1: Write the failing test**

Create `crates/ygg-coding-agent/src/tui/keymap.rs`:
```rust
#![allow(missing_docs)]
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode, mods: KeyModifiers) -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::Key(KeyEvent::new(code, mods))))
    }

    #[test]
    fn ctrl_c_aborts_when_active_and_closes_when_idle() {
        assert!(matches!(translate(key(KeyCode::Char('c'), KeyModifiers::CONTROL), true, true), InputAction::Abort));
        assert!(matches!(translate(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false, true), InputAction::Closed));
    }

    #[test]
    fn enter_submits_idle_and_follows_up_active() {
        // Buffer content is provided by the shell; here we assert the variant only.
        assert!(matches!(translate(key(KeyCode::Enter, KeyModifiers::NONE), false, false), InputAction::Submit(_)));
        assert!(matches!(translate(key(KeyCode::Enter, KeyModifiers::NONE), true, false), InputAction::FollowUp(_)));
    }

    #[test]
    fn empty_enter_is_ignored() {
        assert!(matches!(translate(key(KeyCode::Enter, KeyModifiers::NONE), false, true), InputAction::Ignore));
    }

    #[test]
    fn printable_and_backspace_edit() {
        assert!(matches!(translate(key(KeyCode::Char('x'), KeyModifiers::NONE), false, false), InputAction::Edit(EditAction::Char('x'))));
        assert!(matches!(translate(key(KeyCode::Backspace, KeyModifiers::NONE), false, false), InputAction::Edit(EditAction::Backspace)));
    }

    #[test]
    fn stream_end_closes() {
        assert!(matches!(translate(None, true, true), InputAction::Closed));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent --lib keymap 2>&1 | tail -20`
Expected: FAIL — `translate`, `InputAction`, `EditAction` not found.

- [ ] **Step 3: Write the implementation**

Above the tests:
```rust
pub enum EditAction { Char(char), Backspace, Newline }

pub enum InputAction {
    Abort,
    Steer(String),
    FollowUp(String),
    Submit(String),
    Edit(EditAction),
    Resize(u16, u16),
    Closed,
    Ignore,
}

/// Translate one input event. The shell supplies `active` (a run is in flight) and
/// `pending_is_empty` (the editor buffer). `Submit`/`FollowUp` carry an empty string
/// here; the shell fills the actual buffer text at the call site (it owns the editor).
pub fn translate(
    maybe: Option<Result<Event, std::io::Error>>,
    active: bool,
    pending_is_empty: bool,
) -> InputAction {
    let ev = match maybe {
        Some(Ok(ev)) => ev,
        Some(Err(_)) | None => return InputAction::Closed,
    };
    match ev {
        Event::Resize(c, r) => InputAction::Resize(c, r),
        Event::Key(k) => {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            let alt = k.modifiers.contains(KeyModifiers::ALT);
            match k.code {
                KeyCode::Char('c') if ctrl => if active { InputAction::Abort } else { InputAction::Closed },
                KeyCode::Enter if alt => InputAction::Edit(EditAction::Newline),
                KeyCode::Enter => {
                    if pending_is_empty { InputAction::Ignore }
                    else if active { InputAction::FollowUp(String::new()) }
                    else { InputAction::Submit(String::new()) }
                }
                KeyCode::Backspace => InputAction::Edit(EditAction::Backspace),
                KeyCode::Char(c) if !ctrl => InputAction::Edit(EditAction::Char(c)),
                _ => InputAction::Ignore,
            }
        }
        _ => InputAction::Ignore,
    }
}
```

Add to `tui/mod.rs`: `pub mod keymap;`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib keymap 2>&1 | tail -20`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/keymap.rs crates/ygg-coding-agent/src/tui/mod.rs
git commit -m "feat(coding-agent): pure input keymap translation"
```

---

## Task 8: Minimal InteractiveShell (transcript + editor + status)

**Files:**
- Create: `crates/ygg-coding-agent/src/tui/view.rs`
- Modify: `tui/mod.rs` (`pub mod view;`)
- Test: none automated (TUI rendering needs a TTY); correctness is exercised by Task 10's manual run. Keep the pure state mutations trivial and obvious.

**Interfaces:**
- Consumes: `YggTerminal`, `keymap::{InputAction, EditAction}`, `ygg_agent::{AgentEvent, OutputChannel, Usage}`.
- Produces:
  - `InteractiveShell::enter() -> anyhow::Result<InteractiveShell>` (builds `sexy_tui::TUI` over `YggTerminal`, adds a transcript `Container`, a status line, an input `Markdown` echo).
  - `pub fn on_agent_event(&mut self, ev: &ygg_agent::AgentEvent)`.
  - `pub fn on_turn_finished(&mut self, usage: &ygg_agent::Usage)`.
  - `pub fn apply_edit(&mut self, a: keymap::EditAction)`.
  - `pub fn drain_editor(&mut self) -> String`.
  - `pub fn pending_is_empty(&self) -> bool`.
  - `pub fn set_size(&mut self, cols: u16, rows: u16)`, `pub fn render(&mut self)`, `pub fn set_status(&mut self, s: &str)`, `pub fn leave(self)`.

> Slice 1 keeps the view deliberately simple: the transcript is a single growing `Markdown` block that appends assistant text (and dim reasoning as `> …` quotes), plus one-line tool markers. Rich per-tool panels/overlays land in later slices.

- [ ] **Step 1: Implement `InteractiveShell`**

Create `crates/ygg-coding-agent/src/tui/view.rs`:
```rust
#![allow(missing_docs)]
use sexy_tui::widgets::{Markdown, MarkdownOptions, MarkdownTheme};
use sexy_tui::TUI;
use ygg_agent::{AgentEvent, OutputChannel, Usage};
use crate::tui::keymap::EditAction;
use crate::tui::terminal::YggTerminal;

pub struct InteractiveShell {
    tui: TUI,
    transcript: String,   // rendered as one growing Markdown block (Slice 1)
    editor: String,       // pending input
    status: String,
}

fn md(text: &str) -> Box<Markdown> {
    Box::new(Markdown::new(text, MarkdownTheme::default(), MarkdownOptions::default()))
}

impl InteractiveShell {
    pub fn enter() -> anyhow::Result<Self> {
        let term = YggTerminal::enter()?;
        let mut tui = TUI::new(Box::new(term));
        tui.start();
        let mut s = Self { tui, transcript: String::new(), editor: String::new(), status: "ready".into() };
        s.rebuild();
        Ok(s)
    }

    fn rebuild(&mut self) {
        self.tui.children_mut().clear();
        let body = format!("{}\n\n---\n`{}`\n\n> {}", self.transcript, self.editor, self.status);
        self.tui.add_child(md(&body));
    }

    pub fn on_agent_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::OutputDelta { channel: OutputChannel::Text, text } => self.transcript.push_str(text),
            AgentEvent::OutputDelta { channel: OutputChannel::Reasoning, text } => {
                self.transcript.push_str(&format!("\n> {}", text.replace('\n', "\n> ")));
            }
            AgentEvent::ToolStarted { name, .. } => self.transcript.push_str(&format!("\n\n**⚙ {name}**\n")),
            AgentEvent::ToolFinished { result, .. } => {
                let mark = if result.is_ok() { "✓" } else { "✗" };
                self.transcript.push_str(&format!("\n{mark}\n"));
            }
            _ => {}
        }
        self.rebuild();
    }

    pub fn on_turn_finished(&mut self, usage: &Usage) {
        let occ = usage.total_tokens.saturating_sub(usage.output_tokens);
        self.status = format!("~{occ} ctx tokens");
        self.transcript.push('\n');
    }

    pub fn apply_edit(&mut self, a: EditAction) {
        match a {
            EditAction::Char(c) => self.editor.push(c),
            EditAction::Backspace => { self.editor.pop(); }
            EditAction::Newline => self.editor.push('\n'),
        }
        self.rebuild();
    }

    pub fn drain_editor(&mut self) -> String {
        let t = std::mem::take(&mut self.editor);
        self.transcript.push_str(&format!("\n\n**you:** {t}\n\n"));
        self.rebuild();
        t
    }

    pub fn pending_is_empty(&self) -> bool { self.editor.trim().is_empty() }
    pub fn set_size(&mut self, cols: u16, rows: u16) {
        // The TUI's terminal tracks size; re-query on resize is handled by re-render.
        let _ = (cols, rows);
    }
    pub fn set_status(&mut self, s: &str) { self.status = s.to_string(); self.rebuild(); }
    pub fn render(&mut self) { self.tui.request_render(); }
    pub fn leave(mut self) { self.tui.stop(); }
}
```

> `set_size` in Slice 1 relies on `YggTerminal` size tracking; wire `YggTerminal::set_size` through a shared handle in Slice 6 when overlays/precise layout arrive. `children_mut().clear()` + `add_child` is the confirmed swap pattern.

- [ ] **Step 2: Build to verify the sexy-tui API usage compiles**

Run: `cargo build -p ygg-coding-agent 2>&1 | tail -20`
Expected: `Finished`. Fix any `Markdown`/`TUI` signature mismatches against the rust-port source.

- [ ] **Step 3: Add the module**

Add to `tui/mod.rs`: `pub mod view;`

- [ ] **Step 4: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/view.rs crates/ygg-coding-agent/src/tui/mod.rs
git commit -m "feat(coding-agent): minimal InteractiveShell (transcript/editor/status)"
```

---

## Task 9: Print mode termination classifier + `run_print`

**Files:**
- Create: `crates/ygg-coding-agent/src/modes/mod.rs`
- Create: `crates/ygg-coding-agent/src/modes/print.rs`
- Modify: `main.rs` (`mod modes;`)
- Test: inline in `print.rs` (classifier only; the network path is verified in Task 11 manual run)

**Interfaces:**
- Produces:
  - `pub enum RunEnded { Completed, Aborted, MaxTurns, Failed(String) }` + `From<FinishReason>`.
  - `pub fn classify_finish(finished: Option<RunEnded>) -> anyhow::Result<()>`.
  - `pub async fn run_print(boot: Bootstrap, prompt: String) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing test (classifier)**

Create `crates/ygg-coding-agent/src/modes/print.rs`:
```rust
#![allow(missing_docs)]
use std::io::Write;
use ygg_agent::{AgentEvent, FinishReason, OutputChannel};
use crate::app::bootstrap::{Bootstrap, resolve_launch_print, build_app};

pub enum RunEnded { Completed, Aborted, MaxTurns, Failed(String) }

impl From<FinishReason> for RunEnded {
    fn from(r: FinishReason) -> Self {
        match r {
            FinishReason::Completed => RunEnded::Completed,
            FinishReason::Aborted   => RunEnded::Aborted,
            FinishReason::MaxTurns  => RunEnded::MaxTurns,
            FinishReason::Failed(e) => RunEnded::Failed(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_is_ok_everything_else_errors() {
        assert!(classify_finish(Some(RunEnded::Completed)).is_ok());
        assert!(classify_finish(Some(RunEnded::MaxTurns)).is_err());
        assert!(classify_finish(Some(RunEnded::Aborted)).is_err());
        assert!(classify_finish(Some(RunEnded::Failed("boom".into()))).is_err());
        assert!(classify_finish(None).is_err(), "missing RunFinished is an invariant error");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-coding-agent --lib print 2>&1 | tail -20`
Expected: FAIL — `classify_finish` not found.

- [ ] **Step 3: Implement `classify_finish` and `run_print`**

Above the tests:
```rust
pub fn classify_finish(finished: Option<RunEnded>) -> anyhow::Result<()> {
    match finished {
        Some(RunEnded::Completed) => Ok(()),
        Some(RunEnded::MaxTurns)  => anyhow::bail!("run hit max turns before completing"),
        Some(RunEnded::Aborted)   => anyhow::bail!("run aborted before completing"),
        Some(RunEnded::Failed(e)) => anyhow::bail!("run failed: {e}"),
        None                      => anyhow::bail!("run stream ended without RunFinished (invariant violation)"),
    }
}

pub async fn run_print(boot: Bootstrap, prompt: String) -> anyhow::Result<()> {
    // Slice 1: no compaction gate yet (Slice 3). Resolve launch, build app, stream.
    let stamp = crate::modes::timestamp();
    let launch = resolve_launch_print(&boot, &stamp)?;
    let system = "You are ygg, a concise, local-first coding agent.".to_string(); // AGENTS.md lands in Slice 4
    let mut app = build_app(boot, launch, system)?;

    let show_reasoning = app.config.show_reasoning_in_print;
    let mut run = app.agent.prompt(prompt).await?;
    let mut out = std::io::stdout().lock();
    let mut finished: Option<RunEnded> = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::OutputDelta { channel: OutputChannel::Text, text } => write!(out, "{text}")?,
            AgentEvent::OutputDelta { channel: OutputChannel::Reasoning, text } if show_reasoning => write!(out, "{text}")?,
            AgentEvent::RunFinished { reason, .. } => finished = Some(RunEnded::from(reason)),
            _ => {}
        }
    }
    drop(run);
    out.flush()?;
    classify_finish(finished)
}
```

Create `crates/ygg-coding-agent/src/modes/mod.rs`:
```rust
#![allow(missing_docs)]
pub mod print;

/// A filesystem-safe UTC-ish timestamp for session filenames. Slice 1 uses a
/// monotonic-ish stamp from the system clock; exact formatting is not load-bearing.
pub fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}
```

Add to `main.rs`: `mod modes;`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib print 2>&1 | tail -20`
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/modes crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): print mode with explicit RunFinished classification"
```

---

## Task 10: Interactive loop (idle/active phases, queued control) + main dispatch

**Files:**
- Create: `crates/ygg-coding-agent/src/modes/interactive.rs`
- Modify: `crates/ygg-coding-agent/src/modes/mod.rs` (`pub mod interactive;`)
- Modify: `crates/ygg-coding-agent/src/main.rs` (dispatch)
- Test: none automated (needs a live model + TTY); verified by the manual run in Step 5.

**Interfaces:**
- Consumes: `App`, `InteractiveShell`, `keymap::{translate, InputAction, EditAction}`, `RunEnded`, `bootstrap::{bootstrap, resolve_launch_print, build_app}`, `ygg_agent::{Run, RunControl, AgentEvent}`.
- Produces: `pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()>` and the wired `main`.

- [ ] **Step 1: Implement the loop**

Create `crates/ygg-coding-agent/src/modes/interactive.rs`:
```rust
#![allow(missing_docs)]
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ygg_agent::{AgentError, AgentEvent, Run, RunControl};
use crate::app::bootstrap::{Bootstrap, resolve_launch_print, build_app};
use crate::modes::print::RunEnded;
use crate::tui::keymap::{translate, InputAction, EditAction};
use crate::tui::view::InteractiveShell;

enum ControlIntent { Steer(String), FollowUp(String) }

pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    // Slice 1: model must be resolvable from config (picker is Slice 4). Reuse the
    // print resolver for a New session; interactive resume/pickers land later.
    let stamp = crate::modes::timestamp();
    let launch = resolve_launch_print(&boot, &stamp)
        .map_err(|e| anyhow::anyhow!("{e}\n(interactive model picker arrives in a later slice; pass --model)"))?;
    let system = "You are ygg, a concise, local-first coding agent.".to_string();
    let mut app = build_app(boot, launch, system)?;

    let mut shell = InteractiveShell::enter()?;
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));

    loop {
        // ── Idle phase: full &mut App available ────────────────────────────────
        let submitted = tokio::select! {
            maybe = input.next() => {
                let empty = shell.pending_is_empty();
                match translate(maybe, false, empty) {
                    InputAction::Submit(_) => Some(shell.drain_editor()),
                    InputAction::Closed => break,
                    InputAction::Edit(e) => { shell.apply_edit(e); shell.render(); None }
                    InputAction::Resize(c, r) => { shell.set_size(c, r); shell.render(); None }
                    _ => None,
                }
            }
            _ = ticker.tick() => { shell.render(); None }
        };
        let Some(text) = submitted else { continue };
        if text.trim().is_empty() { continue; }

        // ── Active phase: NO &mut App; only run/control/shell/input/ticker ──────
        let mut run = app.agent.prompt(text).await?;
        let control = run.control();
        let ended = drive_active_run(&mut run, &control, &mut shell, &mut input, &mut ticker).await?;
        drop(run);
        shell.set_status(match &ended {
            RunEnded::Completed => "ready",
            RunEnded::Aborted   => "aborted",
            RunEnded::MaxTurns  => "max turns reached",
            RunEnded::Failed(_) => "error",
        });
    }

    shell.leave();
    Ok(())
}

async fn drive_active_run(
    run: &mut Run<'_>,
    control: &RunControl,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    ticker: &mut tokio::time::Interval,
) -> anyhow::Result<RunEnded> {
    let mut intents: VecDeque<ControlIntent> = VecDeque::new();
    let mut in_flight: Option<Pin<Box<dyn Future<Output = Result<(), AgentError>>>>> = None;

    loop {
        if in_flight.is_none() {
            if let Some(intent) = intents.pop_front() {
                let ctl = control.clone();
                in_flight = Some(Box::pin(async move {
                    match intent {
                        ControlIntent::Steer(t)    => ctl.steer(t).await,
                        ControlIntent::FollowUp(t) => ctl.follow_up(t).await,
                    }
                }));
            }
        }

        tokio::select! {
            biased;
            res = async { in_flight.as_mut().unwrap().await }, if in_flight.is_some() => {
                let _ = res;           // Ok, or Err = run already ended: ignore
                in_flight = None;
            }
            maybe = input.next() => {
                let empty = shell.pending_is_empty();
                match translate(maybe, true, empty) {
                    InputAction::Abort => control.abort(),
                    InputAction::FollowUp(_) => intents.push_back(ControlIntent::FollowUp(shell.drain_editor())),
                    InputAction::Steer(_)    => intents.push_back(ControlIntent::Steer(shell.drain_editor())),
                    InputAction::Edit(e)     => { shell.apply_edit(e); shell.render(); }
                    InputAction::Resize(c,r) => { shell.set_size(c, r); shell.render(); }
                    InputAction::Closed      => { control.abort(); return Ok(RunEnded::Aborted); }
                    InputAction::Submit(_) | InputAction::Ignore => {}
                }
            }
            ev = run.next() => match ev {
                Some(AgentEvent::TurnFinished { usage, .. }) => { shell.on_turn_finished(&usage); shell.render(); }
                Some(AgentEvent::RunFinished { reason, .. }) => return Ok(RunEnded::from(reason)),
                Some(other) => { shell.on_agent_event(&other); shell.render(); }
                None => return Ok(RunEnded::Aborted),
            },
            _ = ticker.tick() => shell.render(),
        }
    }
}
```

Add to `modes/mod.rs`: `pub mod interactive;`

- [ ] **Step 2: Wire `main` dispatch**

Replace `crates/ygg-coding-agent/src/main.rs`:
```rust
#![allow(missing_docs)]
mod cli;
mod config;
mod session_store;
mod app;
mod tui;
mod modes;

use clap::Parser;
use config::Mode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tui::terminal::install_panic_hook();
    let parsed = cli::Cli::parse();
    let cwd = std::env::current_dir()?;
    let config = cli::build_config(parsed, &cwd)?;
    let boot = app::bootstrap::bootstrap(config)?;
    match &boot.config.mode {
        Mode::Print { prompt } => {
            let prompt = prompt.clone();
            modes::print::run_print(boot, prompt).await
        }
        Mode::Interactive => modes::interactive::run_interactive(boot).await,
    }
}
```

- [ ] **Step 3: Build the whole crate**

Run: `cargo build -p ygg-coding-agent 2>&1 | tail -25`
Expected: `Finished`. Resolve any borrow/type errors against the sketches (the `in_flight`/`select!` shape is the load-bearing part).

- [ ] **Step 4: Run all crate unit tests + clippy + fmt**

Run:
```bash
cargo fmt -p ygg-coding-agent
cargo clippy -p ygg-coding-agent --all-targets -- -D warnings 2>&1 | tail -15
cargo test -p ygg-coding-agent 2>&1 | tail -15
```
Expected: fmt clean, clippy clean, all unit tests pass.

- [ ] **Step 5: MANUAL end-to-end vertical-slice verification**

Prereq: an API key in the environment for a builtin-catalog model (e.g. `export ANTHROPIC_API_KEY=…` then use `--model claude-sonnet-4-5`, or `OPENAI_API_KEY` with `--model gpt-4o-mini`).

Print mode (tools + persistence):
```bash
cd /tmp && mkdir -p ygg-demo && cd ygg-demo && git init -q
cargo run -p ygg-coding-agent --bin ygg -- --print --model gpt-4o-mini \
  "create a file hello.txt containing the word ygg, then read it back"
```
Verify: streamed text appears on stdout; `hello.txt` exists with the content; a session JSONL was written under `~/.ygg/sessions/<key>/`; process exits 0. Confirm persistence:
```bash
ls ~/.ygg/sessions/*/*.jsonl && echo "session persisted"
```

Interactive mode (streaming + tools + abort):
```bash
cargo run -p ygg-coding-agent --bin ygg -- --model gpt-4o-mini
```
Verify: type a prompt, press Enter, watch streamed text and a `⚙ tool` marker; press Ctrl+C mid-run to abort cleanly (status shows "aborted", terminal restored); Ctrl+C again (or type + Enter to quit via Closed) exits with a normal shell prompt.

- [ ] **Step 6: Commit**

```bash
git add crates/ygg-coding-agent/src/modes/interactive.rs crates/ygg-coding-agent/src/modes/mod.rs crates/ygg-coding-agent/src/main.rs
git commit -m "feat(coding-agent): interactive idle/active loop + main dispatch — vertical slice reached"
```

---

## Self-Review

**1. Spec coverage (Slice 1 scope).**
- Gate 0 spike with all six pass criteria incl. Ctrl+C-as-key and `TUI::start` non-call → Task 6. ✓
- Workspace resolution (`--workspace | .git | cwd`, canonicalized) → Task 2. ✓
- CLI entry points (`ygg`, `ygg "prompt"`, `--print`, `--continue`, `--resume`, `--model`, `--reasoning`) → Task 3. ✓
- Bootstrap → LaunchSelection → build_app; Theme absent from App; frozen tool reserve via `Tool::definition()` → Task 5. ✓
- Idle/active loop, queued `ControlIntent` + single in-flight send, synchronous abort, explicit `None` handling → Task 10. ✓
- Print mode persistent session + explicit `RunFinished` classification → Task 9. ✓
- Persisted JSONL session, streaming text + reasoning, live tool markers, clean exit → Tasks 8–10 (manual verify). ✓
- Deferred to later slices (correctly absent here): compaction gate, AGENTS.md, named prompts, themes, pickers, transcript hydration, `/commands`. Noted inline.

**2. Placeholder scan.** No "TODO/TBD/handle appropriately"; every code step is complete. Deferred behavior is explicitly named with the owning slice, not left as a placeholder in a Slice-1 code path.

**3. Type consistency.** `RunEnded` is defined once (Task 9, `modes::print`) and reused by Task 10. `InputAction`/`EditAction` (Task 7) are consumed unchanged by Tasks 8/10. `Bootstrap`/`LaunchSelection`/`build_app`/`estimate_text_tokens`/`tool_schema_reserve` (Task 5) are used as declared. `InteractiveShell` method names (`on_agent_event`, `on_turn_finished`, `apply_edit`, `drain_editor`, `pending_is_empty`, `set_size`, `set_status`, `render`, `leave`) match between Task 8 and Task 10. `translate(maybe, active, pending_is_empty)` signature matches its callers. `SandboxConfig` field names (`allow_edit/allow_process/allow_shell/exec_timeout/max_output_bytes`) match the frozen sandbox.

**Known integration risks to watch during execution (not plan gaps):** exact `sexy_tui::Terminal` trait method set and `Markdown`/`MarkdownTheme` constructors on the rust-port branch (Tasks 6/8 Step-2 builds catch mismatches); the `in_flight` `select!` borrow shape (Task 10 Step 3). These are compile-time-caught and localized.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-12-ygg-coding-agent-slice-1.md`. Follow-on slices (2: resume/hydration, 3: compaction gate, 4: resources + pickers + config layers, 5: print/command polish, 6: themes) each get their own plan after this one lands.

Two execution options:

1. **Subagent-Driven (recommended)** — a fresh subagent per task, review between tasks, fast iteration. Note: Task 6 Step 6 and Task 10 Step 5 are **manual, human-run gates** (a real TTY and an API key are required); a subagent cannot complete them and must hand back for you to run.
2. **Inline Execution** — execute tasks in this session with checkpoints for review, pausing at the two manual gates.

Which approach?
