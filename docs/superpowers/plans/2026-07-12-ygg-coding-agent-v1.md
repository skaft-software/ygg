# ygg-coding-agent — Complete v1 Implementation Plan

> **Execution mode:** Single-agent, inline execution. Implement this plan task-by-task, checking off each step as it's completed. Steps use checkbox (`- [ ]`) syntax. This plan **supersedes** `2026-07-12-ygg-coding-agent-slice-1.md`.

**Goal:** Ship a genuinely daily-usable, full-screen interactive coding agent (`ygg`) built on `sexy-tui-rs` over the frozen `ygg-agent`: launch → create/resume a persistent session → hydrate transcript → stream thinking + assistant text → run read/search/edit/exec tools with live output → steer/queue/abort → change model/thinking/theme → auto+manual compaction → durable state → clean exit → resume later. Print mode is a secondary headless frontend.

**Architecture:** Procedural shell over the frozen `Agent`. A mode-agnostic `App` (holds `agent`, `model`, `client`, `config`, `catalog`, `sessions`, `reasoning`, `system`, token estimates); an `InteractiveShell` (all terminal I/O + Theme). One `#[tokio::main(flavor="current_thread")]` runtime. Interactive loop has explicit **idle** (full `&mut App`; commands, reconfig, compaction gate) and **active** (`drive_active_run`: run/control/shell/input/ticker only) phases, honoring `Run<'a>`'s `&mut Agent` borrow. Runtime `/model /thinking /new /resume` use a **consuming `rebuild_app`** transition (drop old `Agent` before reopening the session). Normative design: `docs/superpowers/specs/2026-07-12-ygg-coding-agent-design.md` (Revision 3).

**Tech Stack:** Rust 2021, tokio (current-thread), crossterm 0.28 (event-stream), clap 4 (derive), sexy-tui-rs (pinned Git dep at verified rust-port commit; local-path override for developers), anyhow, serde/toml, dirs, tokio-stream; frozen `ygg-ai` + `ygg-agent`.

## Global Constraints

- Rust edition **2021**, `rust-version = 1.80` (inherit `[workspace.package]`).
- **Do not modify** `crates/ygg-ai` or `crates/ygg-agent`. Public APIs only. The reasoning-config fix (`AgentConfig.reasoning`) is already in the worktree. **All existing frozen tests remain green** — `cargo test -p ygg-ai -p ygg-agent` passes clean; no frozen test is removed, ignored, or weakened. Report actual counts at each gate rather than relying on a single historical number.
- Interactive runtime is **`#[tokio::main(flavor = "current_thread")]`**. **Never** call `sexy_tui::ProcessTerminal::start()`.
- **No second turn/tool loop, no event bus, no actor system, no `Arc<Mutex<AppState>>`.** The only direct `AiClient::complete` use is the stateless compaction summarizer.
- Control sends are **queued**, never awaited inside the input arm of the active loop.
- Runtime `/model /thinking /new /resume` only at an **idle boundary**, via the consuming `rebuild_app` (drop old `Agent` before reopening the session — never two owners).
- **v1 commands:** `/model /thinking /theme /compact /new /resume /status /help /quit`. **No `/checkout`, no `/prompt`.** `AGENTS.md` is the only project-instruction mechanism. No named-prompt subsystem.
- **OAuth / subscription login (OpenAI Codex, Anthropic Pro/Max, GitHub Copilot) is deferred to post-v1.** `ygg-ai` has the `Auth::Dynamic` + `CredentialResolver` infrastructure ready; a concrete resolver and Codex catalog entries are the remaining work. See `docs/plans/ygg-ai-oauth-codex.md`.
- **Active-slash path:** when the editor is drained during an active run and the text begins with `/`, it is parsed as an application command — never forwarded to the model. **Active-safe commands** (`/status`, `/help`, `/theme`) execute immediately without touching the Agent. **Queued commands** (`/model`, `/thinking`, `/new`, `/resume`, `/compact`) are pushed onto a `VecDeque<PendingIdleAction>` and applied in submission order at the next idle boundary via the consuming `rebuild_app` transition. `/quit` during an active run aborts the run, drains to `RunFinished`, restores the terminal, and exits.
- **Queue coalescing rule:** adjacent `ChangeModel` actions may be coalesced (only the last survives). Adjacent `ChangeThinking` actions may be coalesced (only the last survives). All other actions (`NewSession`, `ResumeSession`, `Compact`) must be preserved in order; they must never be silently dropped by a later push. Example: `/model A`, `/model B`, `/thinking high` may become `[ChangeModel(B), ChangeThinking(high)]`, but `/new`, `/model B` must remain `[NewSession, ChangeModel(B)]`.
- Tool-schema reserve reads the frozen `Tool::definition()` of `ReadTool/SearchTool/EditTool/ExecTool`. **Never reproduce tool JSON schemas.**
- sexy-tui: `MarkdownTheme`/`SelectListTheme` have **no `Default`** — build via closure helpers (the `EditorTheme::new(&Theme)` pattern). Pickers drive `SelectList::handle_input(&str)` with a **replicated key encoder** and read `selected_item()`. Terminal size is shared via `Rc<Cell<(u16,u16)>>`.
- TDD for all pure-logic units. Frequent commits (≥1 per task). DRY. YAGNI. Binary crate: `#![allow(missing_docs)]` at crate root.
- Two **manual, human-run gates** (real TTY, and one live provider): M1 Gate 0, and M14 acceptance. These gates require a real terminal and live API credentials — the agent will pause at these points for manual verification.

---

## Milestone / file map

| Milestone | Files (primary) |
|---|---|
| M1 Gate 0 | `spike/bin_spike.rs`, `tui/terminal.rs` |
| M2 Foundation | `Cargo.toml`, `main.rs`, `cli.rs`, `config.rs`, `session_store.rs` |
| M3 Core shell | `tui/{mod,terminal,keymap,view,theme}.rs`, `app/{mod,bootstrap}.rs`, `modes/interactive.rs` |
| M4 Steering/abort | `modes/interactive.rs` (`drive_active_run`) |
| M5 Print mode | `modes/print.rs` |
| M6 Sessions/hydration | `session_store.rs`, `tui/view.rs` (`hydrate`), `modes/interactive.rs`, `tui/pickers.rs` |
| M7 `/model` | `app/mod.rs` (`rebuild_app`), `tui/pickers.rs`, `commands.rs` |
| M8 `/thinking` | `config.rs` (`ThinkingLevel`), `app/mod.rs`, `commands.rs` |
| M9 Compaction | `compaction.rs` |
| M10 AGENTS.md/config | `resources.rs`, `config.rs` |
| M11 Themes | `tui/theme.rs`, `commands.rs` |
| M12 status/help/quit | `commands.rs`, `tui/view.rs` |
| M13 Regression | tests across crate |
| M14 Acceptance | manual |

Reused verified types: `RunEnded` (M5, `modes::print`), `InputAction`/`EditAction` (M3, `tui::keymap`), `Reconfig` (M7), `CapacityDecision`/`CompactionOutcome` (M9).

---

# Milestone 1 — Gate 0: real-TTY sexy-tui integration (HARD GATE)

> Manual, run-it-yourself gate. If any criterion fails, STOP — the interactive architecture is invalidated (fallback: dedicated blocking-input thread bridged by a channel, or a non-blocking sexy-tui driver). This is the **only** architecture-changing implementation risk.

## Task 1.1: Materialize sexy-tui-rs and scaffold the crate

**Files:** `Cargo.toml` (workspace), `crates/ygg-coding-agent/Cargo.toml`, `crates/ygg-coding-agent/src/main.rs`

**Interfaces:** Produces a compiling `ygg` binary; sexy-tui-rs builds from a pinned Git dependency (not a bare path).

- [ ] **Step 1: Build the rust-port checkout and capture its HEAD commit.**
```bash
git -C ~/github/achuthanmukundan00/sexy-tui-rs fetch origin
# Capture the HEAD commit of the rust-port branch:
FULL_COMMIT_SHA=$(git -C ~/github/achuthanmukundan00/sexy-tui-rs rev-parse origin/rust-port)
cargo build --manifest-path ~/github/achuthanmukundan00/sexy-tui-rs/Cargo.toml
```
Expected: sexy-tui-rs builds clean. Substitute `$FULL_COMMIT_SHA` for `<FULL_COMMIT_SHA>` in `Cargo.toml`. The committed manifest contains only `git` and `rev` (no `branch`, `tag`, or `path`). If the upstream is unavailable, fall back to a path override documented in a `[patch]` comment.

- [ ] **Step 2: Add the workspace member** in `Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/ygg-ai", "crates/ygg-agent", "crates/ygg-coding-agent"]
```

- [ ] **Step 3: Create `crates/ygg-coding-agent/Cargo.toml`:**
```toml
[package]
name = "ygg-coding-agent"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Full-screen interactive coding agent over ygg-agent."

[lints]
workspace = true

[[bin]]
name = "ygg"
path = "src/main.rs"

[[bin]]
name = "ygg-tui-spike"
path = "src/spike/bin_spike.rs"

[dependencies]
ygg-ai = { path = "../ygg-ai" }
ygg-agent = { path = "../ygg-agent" }
# Pinned to the verified rust-port commit from Step 1.
# Developer override: `[patch.'https://github.com/achuthanmukundan00/sexy-tui-rs']
# sexy-tui-rs = { path = "../../../../achuthanmukundan00/sexy-tui-rs-rust-port" }`
sexy-tui-rs = { git = "https://github.com/achuthanmukundan00/sexy-tui-rs", rev = "<FULL_COMMIT_SHA>" }
tokio = { version = "1", features = ["rt", "macros", "time", "io-util", "process"] }
crossterm = { version = "0.28", features = ["event-stream"] }
futures-util = "0.3"
tokio-stream = "0.1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
dirs = "5"
anyhow = "1"

[dev-dependencies]
tempfile = "3"
tokio-test = "0.4"
```
> Substitute the `FULL_COMMIT_SHA` from Step 1 for `<FULL_COMMIT_SHA>`. The only specifiers are `git` and `rev` — never `branch`, `tag`, or `path` in the committed manifest. `tokio-stream` provides `ReceiverStream` for the channel-backed test input in M13. `tokio-test` provides test utilities.

- [ ] **Step 4: Create `crates/ygg-coding-agent/src/main.rs`:**
```rust
#![allow(missing_docs)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> { println!("ygg scaffold"); Ok(()) }
```

- [ ] **Step 5: Build + confirm frozen crates untouched.**
```bash
cargo build -p ygg-coding-agent && cargo test -p ygg-ai -p ygg-agent 2>&1 | grep "test result" | tail -3
```
Expected: build `Finished`; all existing ygg-ai and ygg-agent tests pass. Record and report the actual frozen test count.

- [ ] **Step 6: Commit.** `git add -A && git commit -m "feat(coding-agent): scaffold crate + pinned sexy-tui-rs dep"`

## Task 1.2: `YggTerminal` (shared-size, idempotent restore, init-failure rollback) + spike

**Files:** `crates/ygg-coding-agent/src/tui/mod.rs`, `crates/ygg-coding-agent/src/tui/terminal.rs`, `crates/ygg-coding-agent/src/spike/bin_spike.rs`

**Interfaces:**
- `tui::terminal::{YggTerminal, force_restore, install_panic_hook}`.
- `YggTerminal::enter() -> anyhow::Result<(YggTerminal, Rc<Cell<(u16,u16)>>)>` — the second element is the size cell the shell updates on resize.
- `impl sexy_tui::Terminal for YggTerminal` (render-only; `start()` is `unreachable!()`).
- **Rollback invariant:** if any operation after `enable_raw_mode()` fails (e.g., `EnterAlternateScreen`, `cursor::Hide`), the terminal must be restored before the error is returned. The restoration guard is set immediately after raw mode is enabled and is idempotent; `force_restore()` is called in every error-return path.

- [ ] **Step 1: Write the idempotent-restore unit test** in `terminal.rs`:
```rust
#![allow(missing_docs)]
use std::cell::Cell;
use std::io::{Stdout, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use crossterm::{cursor, execute, terminal};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn force_restore_is_idempotent_without_a_terminal() {
        RAW_ACTIVE.store(false, Ordering::SeqCst);
        force_restore(); force_restore();
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
    }
}
```
- [ ] **Step 2: Run it — fails** (`RAW_ACTIVE`/`force_restore` not found): `cargo test -p ygg-coding-agent --lib terminal 2>&1 | tail`.
- [ ] **Step 3: Implement** above the tests:
```rust
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn force_restore() {
    if RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = out.flush();
    }
}
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { force_restore(); prev(info); }));
}

pub struct YggTerminal { out: Stdout, size: Rc<Cell<(u16, u16)>> }

impl YggTerminal {
    pub fn enter() -> anyhow::Result<(Self, Rc<Cell<(u16, u16)>>)> {
        // Enter raw mode first, then immediately arm the restoration guard so
        // any subsequent failure restores the terminal before propagating.
        terminal::enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);

        // Use a helper so every error path calls force_restore().
        let result = Self::enter_inner();
        if result.is_err() {
            force_restore();
        }
        result
    }

    fn enter_inner() -> anyhow::Result<(Self, Rc<Cell<(u16, u16)>>)> {
        let mut out = std::io::stdout();
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        let (c, r) = terminal::size().unwrap_or((80, 24));
        let size = Rc::new(Cell::new((c, r)));
        Ok((Self { out, size: size.clone() }, size))
    }
}
impl Drop for YggTerminal { fn drop(&mut self) { force_restore(); } }

impl sexy_tui::Terminal for YggTerminal {
    fn start(&mut self, _in: Box<dyn FnMut(&str)>, _rs: Box<dyn FnMut()>) {
        unreachable!("YggTerminal::start is never called; input is driven by the select! loop");
    }
    fn write(&mut self, data: &str) { let _ = self.out.write_all(data.as_bytes()); let _ = self.out.flush(); }
    fn columns(&self) -> u16 { self.size.get().0 }
    fn rows(&self) -> u16 { self.size.get().1 }
    fn move_by(&mut self, lines: i16) {
        let _ = if lines > 0 { execute!(self.out, cursor::MoveDown(lines as u16)) }
                else if lines < 0 { execute!(self.out, cursor::MoveUp((-lines) as u16)) } else { Ok(()) };
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
```
Create `tui/mod.rs`: `#![allow(missing_docs)]\npub mod terminal;`. Add `mod tui;` to `main.rs`.
> Verify the `Terminal` trait method set matches the rust-port `src/terminal.rs`. A missing/extra method is a compile error in Step 6.

- [ ] **Step 4: Test passes** (`cargo test -p ygg-coding-agent --lib terminal`). Add a test that verifies `enter()` restores the terminal when a post-`enable_raw_mode` step fails: after a simulated alternate-screen failure, `RAW_ACTIVE` is false and the terminal is usable.
- [ ] **Step 5: Write the spike** `crates/ygg-coding-agent/src/spike/bin_spike.rs`:
```rust
#![allow(missing_docs)]
use std::time::Duration;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use sexy_tui::widgets::{Markdown, MarkdownOptions};

#[path = "../tui/terminal.rs"]
mod terminal;
use terminal::{YggTerminal, force_restore, install_panic_hook};

// Minimal MarkdownTheme (no Default upstream): identity closures suffice for the spike.
fn plain_md_theme() -> sexy_tui::widgets::MarkdownTheme {
    use sexy_tui::widgets::MarkdownTheme;
    let id = || -> Box<dyn Fn(&str) -> String> { Box::new(|s: &str| s.to_string()) };
    MarkdownTheme {
        heading: id(), bold: id(), italic: id(), code: id(), code_block_border: id(),
        code_block_bg: id(), link: id(), link_url: id(), quote: id(), quote_border: id(),
        hr: id(), list_bullet: id(), strikethrough: id(), underline: id(), highlight_code: None,
    }
}
fn md(text: &str) -> Box<Markdown> { Box::new(Markdown::new(text, plain_md_theme(), MarkdownOptions::default())) }

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    install_panic_hook();
    let (term, _size) = YggTerminal::enter()?;
    let mut tui = sexy_tui::TUI::new(Box::new(term));
    tui.add_child(md("# spike\ntype; `q` or Ctrl+C quits; resize the window."));
    tui.start(); // MUST NOT reach YggTerminal::start (unreachable!())
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut buf = String::from("# spike\n");
    loop {
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(Event::Key(k))) if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) => break,
                Some(Ok(Event::Key(k))) if k.code == KeyCode::Char('q') => break,
                Some(Ok(Event::Key(k))) => { if let KeyCode::Char(c) = k.code { buf.push(c); tui.children_mut().clear(); tui.add_child(md(&buf)); tui.request_render(); } }
                Some(Ok(Event::Resize(_, _))) => tui.request_render(),
                Some(Err(_)) | None => break,
                _ => {}
            },
            _ = ticker.tick() => tui.request_render(),
        }
    }
    tui.stop();
    force_restore();
    Ok(())
}
```
> `MarkdownTheme` field list is from `widgets/markdown.rs` (15 fields incl. `highlight_code: Option<...>`). If the field set differs on your checkout, match it exactly — the build catches mismatches.

- [ ] **Step 6: Build the spike.** `cargo build -p ygg-coding-agent --bin ygg-tui-spike 2>&1 | tail -20`. Fix API mismatches until `Finished`.

- [ ] **Step 7: MANUAL GATE — run in a real terminal.** `cargo run -p ygg-coding-agent --bin ygg-tui-spike`. Verify ALL:
  1. Renders/updates on typing, no `ProcessTerminal::start()`.
  2. Ran past `tui.start()` without panic → `TUI::start` does not call `Terminal::start`.
  3. Repaints on the 80 ms ticker with no input (non-input async wakeup).
  4. Resize reflows without corruption.
  5. `q`, **Ctrl+C (raw key, not signal)**, and a forced panic all restore the terminal (normal shell prompt after).
  6. Ran under `current_thread` (no `Send` errors).
  7. **Binding verification — confirm crossterm delivers the three load-bearing chords in raw mode on the test terminal:**
     - **Ctrl+S** arrives as `KeyEvent { code: Char('s'), modifiers: CONTROL }` (not swallowed as XON flow control).
     - **Alt+Enter** arrives as `KeyEvent { code: Enter, modifiers: ALT }` (distinct from unmodified Enter).
     - **Ctrl+C** arrives as `KeyEvent { code: Char('c'), modifiers: CONTROL }` (not delivered as SIGINT).
     Print each event to stdout from the spike loop and verify manually. If any chord is not delivered as specified, document the terminal emulator and the actual encoding — the keymap must accommodate it before the active loop is built.
- [ ] **Step 8: Record result + commit.** If all pass: `git add -A && git commit -m "feat(coding-agent): Gate 0 spike + YggTerminal — gate PASSED"`. If any fail, STOP and report.

**Acceptance (M1):** spike passes all six criteria in a real TTY; frozen tests green; init-failure rollback test passes.

---

# Milestone 2 — Foundation: launcher, workspace, config, session paths

## Task 2.1: Workspace resolution (`config.rs`)

**Interfaces:** `pub fn resolve_workspace(explicit: Option<&Path>, cwd: &Path) -> std::io::Result<PathBuf>` — explicit → nearest `.git` ancestor → cwd; canonicalized.

- [ ] **Step 1: Failing tests** (`config.rs`): explicit-wins, nearest-`.git`, cwd-fallback — each asserts `== dir.canonicalize()`. (Same three tests as the superseded slice-1 Task 2.)
- [ ] **Step 2: Run — fails** (`resolve_workspace` missing).
- [ ] **Step 3: Implement:**
```rust
pub fn resolve_workspace(explicit: Option<&Path>, cwd: &Path) -> std::io::Result<PathBuf> {
    if let Some(p) = explicit { return p.canonicalize(); }
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        if dir.join(".git").exists() { return dir.canonicalize(); }
        cur = dir.parent();
    }
    cwd.canonicalize()
}
```
Add `mod config;` to `main.rs`.
- [ ] **Step 4: Pass.** **Step 5: Commit** `feat(coding-agent): workspace resolution`.

## Task 2.2: `Config`, `Cli`, `build_config` (`config.rs`, `cli.rs`)

**Interfaces:** `Config`, `Mode::{Interactive, Print{prompt}}`, `ResumeSelector::{New,Continue,Resume(Option<String>)}`, `SandboxPolicy`, `CompactionPolicy`, `parse_reasoning`, `default_session_dir`; `cli::Cli` (clap), `cli::build_config(Cli, &Path) -> anyhow::Result<Config>`.

Includes fields the whole product needs: `workspace`, `invocation_cwd`, `model: Option<ModelId>`, `reasoning: ReasoningConfig`, `sandbox`, `theme: Option<String>`, `session_dir`, `max_turns`, `show_reasoning_in_print`, `mode`, `resume`.

- [ ] **Step 1: Failing tests** (`cli.rs`): print-requires-prompt errors; print builds `Mode::Print`; `--continue` → `ResumeSelector::Continue` + `Mode::Interactive`; `parse_reasoning` accepts `off/high/budget=2048`, rejects `nonsense`. (Bodies as in the superseded slice-1 Task 3, plus a `theme` field default `None`.)
- [ ] **Step 2: Run — fails.**
- [ ] **Step 3: Implement** `Config`/enums/`parse_reasoning`/`default_session_dir` in `config.rs` and `Cli`/`build_config` in `cli.rs` (per design §5.1 + §8; the slice-1 Task 3 code is correct — copy it, adding `theme: cli.theme` and a `--theme` flag). Add `mod cli;`.
- [ ] **Step 4: Pass.** **Step 5: Commit** `feat(coding-agent): CLI + layered Config (defaults+CLI)`.

## Task 2.3: `SessionStore` (`session_store.rs`)

**Interfaces:** `SessionStore::new(session_dir,&Path workspace)`, `dir()`, `new_path(stamp:&str)->PathBuf`, `list()->Vec<SessionMeta>`, `latest()->Result<SessionMeta>`, `by_id(id:&str)->Result<SessionMeta>`; `SessionMeta{id,path,modified,title}`.

- [ ] **Step 1: Failing tests:** per-workspace dirs stable & distinct; `new_path` inside `dir()` with `.jsonl` and `stamp-` prefix; `latest()` newest by mtime. (As in slice-1 Task 4.)
- [ ] **Step 2: Run — fails. Step 3: Implement** (slice-1 Task 4 code: FNV workspace key, `new_path`, `list` sorted newest-first with empty title, `latest`). Add `by_id(id)` = `list().into_iter().find(|m| m.id == id).ok_or(...)`. Add `mod session_store;`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): SessionStore path layout`.

**Acceptance (M2):** `cargo test -p ygg-coding-agent` green; `ygg --help` shows the launcher flags.

---

# Milestone 3 — Core interactive shell + event mapping

## Task 3.1: Theme helpers (`tui/theme.rs`)

**Interfaces:** `pub fn load_theme(cfg: &Config) -> sexy_tui::Theme`; `pub fn markdown_theme(t: &Theme) -> MarkdownTheme`; `pub fn select_list_theme(t: &Theme) -> SelectListTheme`. Built via the `EditorTheme::new(&Theme)` closure pattern (clone `Theme` into `move` closures calling `t.fg(token,s)`/`t.bold(s)`).

- [ ] **Step 1: Failing test:** `markdown_theme(&Theme::default())` builds and a closure round-trips text (`(theme.bold)("x")` contains `"x"`).
- [ ] **Step 2: Run — fails. Step 3: Implement** the three helpers (each constructs the struct from cloned-Theme closures; `MarkdownTheme` has 15 fields, `SelectListTheme` 5 — use `t.fg("accent",s)`, `t.fg("muted",s)`, `t.bold(s)`, `t.dim(s)` appropriately; `highlight_code: None` for v1). `load_theme` = `Theme::load(cfg.theme.as_deref().map(theme_path)…, CapabilityTier::Baseline)`; failure → `Theme::default()` (never fatal). Add `pub mod theme;` to `tui/mod.rs`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): sexy-tui theme helpers (no Default upstream)`.

## Task 3.2: Keymap translation + key encoder (`tui/keymap.rs`)

**Interfaces:** `InputAction{Abort,Steer(String),FollowUp(String),Submit(String),Command(String),Edit(EditAction),Resize(u16,u16),Closed,Ignore}`, `EditAction{Char(char),Backspace,Newline}`, `pub fn translate(maybe: Option<Event>, active: bool, editor_text: &str) -> InputAction`, `pub fn encode(k: &KeyEvent) -> String` (replicates the private `key_to_string`).

**Concrete v1 key bindings:**

| Chord | Mode | Action |
|---|---|---|
| Enter (idle, editor starts `/`) | idle | `Command(editor_text)` — parsed at the call site |
| Enter (idle, normal text) | idle | `Submit(editor_text)` — sends the prompt to the model |
| Alt+Enter | idle | `Edit(Newline)` — inserts a newline in the multiline editor |
| Ctrl+S (active, editor starts `/`) | active | `Command(editor_text)` — parsed as an application command, never sent to the model |
| Ctrl+S (active, normal text) | active | `Steer(editor_text)` — immediate steer injected at the next model-turn boundary |
| Enter (active, editor starts `/`) | active | `Command(editor_text)` — parsed as an application command, never sent to the model |
| Enter (active, normal text) | active | `FollowUp(editor_text)` — queued follow-up for after the current turn settles |
| Ctrl+C (active) | active | `Abort` |
| Ctrl+C (idle) | idle | `Closed` — exit the application |
| PageUp / PageDown | any | scroll transcript viewport (handled by the shell directly; not a keymap action) |
| Esc | picker/overlay | `Close` — dismiss the active picker or overlay |
| printable char, Backspace | any | `Edit(Char(c))` / `Edit(Backspace)` |
| stream exhausted | any | `Closed` |

**Active-slash rule in `translate`:** when the editor buffer (passed as `editor_text`) begins with `/`, any submit key (Enter, Ctrl+S) returns `Command(editor_text)` regardless of the active/idle mode. This guarantees application commands are never accidentally forwarded to the model as steer or follow-up text.

- [ ] **Step 1: Failing tests:** Ctrl+C → Abort(active)/Closed(idle); Enter (idle, normal) → `Submit`; Enter (active, normal) → `FollowUp`; Ctrl+S (active, normal) → `Steer`; Ctrl+S (idle) → `Ignore`; Enter (any, buffer starts `/`) → `Command`; Ctrl+S (any, buffer starts `/`) → `Command`; Alt+Enter → `Edit(Newline)`; empty Enter → `Ignore`; printable/Backspace → `Edit`; stream end → `Closed`; **`encode`**: `Up`→`"\x1b[A"`, `Enter`→`"\r"`, `Esc`→`"\x1b"`, `Char('x')`→`"x"`, `Ctrl+Char('a')`→`"\x1b[97;5u"`.
- [ ] **Step 2: Run — fails. Step 3: Implement.** `translate` checks `editor_text` for the `/` prefix first (returning `Command` for any submit chord when it matches), then dispatches by mode and chord. `encode` mirrors the verified `key_to_string` (arrows `\x1b[A/B/C/D`, Home/End/PageUp/… , Enter `\r`, Tab `\t`, Backspace `\x7f`, Esc `\x1b`, kitty `\x1b[{cp};{mod}u` when modified). Add `pub mod keymap;`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): keymap translation + key encoder`.

## Task 3.3: `App`, bootstrap, `build_app` (`app/mod.rs`, `app/bootstrap.rs`)

**Interfaces:** `App{agent,model,client,config,catalog,sessions,reasoning,system,system_tokens,tool_schema_tokens}`; `Bootstrap{config,catalog,sessions,client}`; `LaunchSelection{model,session}`, `SessionSelection::{OpenExisting,CreateNew}`; `bootstrap(Config)`, `resolve_launch_print(&Bootstrap,stamp)`, `build_app(Bootstrap,LaunchSelection,String)`, `estimate_text_tokens`, `tool_schema_reserve`, `reasoning_label`.

- [ ] **Step 1: Failing tests** (bootstrap.rs): print-launch errors without model; creates new-session path with model; `tool_schema_reserve()>0` and deterministic. (As slice-1 Task 5, but `App` now carries `catalog`/`sessions`/`reasoning`/`system`.)
- [ ] **Step 2: Run — fails. Step 3: Implement** per design §5.2 (the corrected `build_app` that retains `catalog`+`sessions`+`reasoning`+`system`; `tool_schema_reserve` collects `ToolDef` values by calling `.definition()` on the concrete unit structs — no heterogeneous trait-object array, no schema duplication):
```rust
use ygg_agent::tools::{ReadTool, SearchTool, EditTool, ExecTool};

fn tool_schema_reserve() -> usize {
    let defs: Vec<ygg_ai::ToolDef> = vec![
        ReadTool.definition(),
        SearchTool.definition(),
        EditTool.definition(),
        ExecTool.definition(),
    ];
    // Count tokens from the serialized JSON of the combined ToolDefs.
    // Each ToolDef carries name, description, parameters — the JSON schemas
    // live inside the tool implementations and are never reproduced here.
    let json = serde_json::to_string(&defs).unwrap();
    estimate_text_tokens(&json)
}
```
Add `reasoning_label(&ReasoningConfig)->String` (`"off"|"minimal"|…|"budget=N"`). Add `mod app;`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): App + bootstrap + build_app (catalog/sessions retained)`.

## Task 3.4: `InteractiveShell` + `AgentEvent` mapping (`tui/view.rs`)

**Interfaces:** `InteractiveShell::enter(theme:Theme, size:Rc<Cell<(u16,u16)>>) -> Result<Self>`; `on_agent_event(&mut self,&AgentEvent)`; `on_turn_finished(&mut self,&Usage)`; `apply_edit(&mut self,EditAction)`; `set_status(&mut self,&str)`; `set_size(&mut self,u16,u16)` (writes the cell); `pending_is_empty(&self)->bool`; `pending(&self)->&str`; `drain_editor(&mut self)->String`; `render(&mut self)`; `leave(self)`; plus transcript/tool-panel state with **bounded 64 KiB/panel**; separate reasoning block.

- [ ] **Step 1: Implement** the shell (design §5.3, corrected): owns `sexy_tui::TUI` over `YggTerminal`, a growing transcript with per-`ToolCallId` panels (bounded ring), a status line, an editor buffer; `set_size` writes the shared `Rc<Cell>` then re-renders; mapping per design §5.3 table (Text→append; Reasoning→dim block; ToolStarted→panel; ToolProgress→bounded append; ToolFinished→finalize; TurnFinished→gauge from `total-output`). Use `markdown_theme(&theme)` (M3.1) for rendering.
- [ ] **Step 2: Build** to verify sexy-tui usage compiles (`cargo build -p ygg-coding-agent`). Fix `Markdown`/`TUI` mismatches against the rust-port.
- [ ] **Step 3:** Add a pure unit test for `bounded_append` (the 64 KiB ring helper: appending >64 KiB keeps the tail and an elision marker). **Step 4: Pass. Step 5: Commit** `feat(coding-agent): InteractiveShell + AgentEvent mapping + bounded panels`.

**Acceptance (M3):** crate builds; shell unit tests pass; theme/keymap/encoder tests pass.

---

# Milestone 4 — Steering, queued follow-ups, abort, backpressure

## Task 4.1: `drive_active_run` + interactive loop skeleton (`modes/interactive.rs`)

**Interfaces:** `ControlIntent{Steer(String),FollowUp(String)}`; `PendingIdleAction` enum (variants for each reconfig-triggering command: `ChangeModel(ModelId)`, `ChangeThinking(ReasoningConfig)`, `NewSession`, `ResumeSession(Option<String>)`, `Compact`); `run_interactive(Bootstrap) -> Result<()>`; `drive_active_run<S>(&mut Run, &RunControl, &mut InteractiveShell, input: &mut S, ticker: &mut Interval) -> Result<RunEnded>` where `S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin`. (`RunEnded` from M5 lands first if needed; define it here in `modes::print` and `use` it, or temporarily in `interactive` then move — plan orders M5 before wiring exit-codes; define `RunEnded` in `modes/mod.rs` to avoid churn.)

> Place `RunEnded` + `From<FinishReason>` in `modes/mod.rs` so both modes share it.
> The generic `S` bound on `drive_active_run` is the testing seam: production passes `&mut crossterm::event::EventStream`; M13 tests pass a channel-backed stream (`tokio::sync::mpsc::Receiver<Result<Event>>` wrapped via `tokio_stream::wrappers::ReceiverStream`).

- [ ] **Step 1: Implement** the idle/active loop and `drive_active_run` exactly per design §6.2 (queued `ControlIntent`, single in-flight `Pin<Box<dyn Future<Output=Result<(),AgentError>>>>` owning a cloned `RunControl`; `biased` select over in-flight/input/run/ticker; synchronous `control.abort()`; explicit `None`→`Aborted`). Idle phase: submit → prompt; `/`-command → dispatch (stub `run_command` returns Ok for now, filled per milestone). During the active phase, when a `Command` action is received from the keymap: parse the command; active-safe commands (`/status`, `/help`, `/theme`) execute immediately without touching the Agent; queued commands (`/model`, `/thinking`, `/new`, `/resume`, `/compact`) are pushed onto a `pending_actions: VecDeque<PendingIdleAction>` and applied in submission order after the run settles (adjacent model/thinking changes may coalesce — see Global Constraints); `/quit` aborts the run, drains to `RunFinished`, then exits (application commands reach the handler via the `Command` action from the keymap, never as model input). Model resolution reuses `resolve_launch_print` for a New session (interactive picker arrives M6/M7). `system` = a base persona string for now (AGENTS.md lands M10).
- [ ] **Step 2: Build.** Resolve the `in_flight` select borrow shape (the load-bearing part). Verify the generic `S` bound compiles with `&mut crossterm::event::EventStream`.
- [ ] **Step 3: Wire `main` dispatch** (`Mode::Print`→`run_print` stub for now; `Mode::Interactive`→`run_interactive`); `install_panic_hook()` first.
- [ ] **Step 4: fmt + clippy + test.** `cargo fmt -p ygg-coding-agent && cargo clippy -p ygg-coding-agent --all-targets -- -D warnings && cargo test -p ygg-coding-agent`.
- [ ] **Step 5: Commit** `feat(coding-agent): idle/active loop with queued control sends + active-slash dispatch`.

**Acceptance (M4):** builds; clippy/fmt clean; unit tests pass. Generic `S` bound compiles with `EventStream`. (Live behavior verified at M14; a channel-backed scripted test is added at M13.)

---

# Milestone 5 — Persistent print mode (secondary)

## Task 5.1: `run_print` + `classify_finish` (`modes/print.rs`)

**Interfaces:** `RunEnded` (in `modes/mod.rs`), `classify_finish(Option<RunEnded>)->Result<()>`, `run_print(Bootstrap,String)->Result<()>`.

- [ ] **Step 1: Failing test** (`print.rs`): `classify_finish` — `Completed`→Ok; `MaxTurns`/`Aborted`/`Failed`/`None`→Err (per design §6.7).
- [ ] **Step 2: Run — fails. Step 3: Implement** `classify_finish` + `run_print` per design §6.7 (resolve_launch_print → build_app → **capacity gate is added in M9**; for M5 stream directly; copy `show_reasoning` before the `Run` borrow; track `RunFinished`; `out.flush()`; `classify_finish`).
> Note: the capacity gate call (`ensure_capacity_before_prompt`) is inserted here in M9; for M5 leave a `// M9: capacity gate` marker and stream directly.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): persistent print mode + RunFinished classification`.

**Acceptance (M5):** `classify_finish` tests pass; `ygg --print --model <id> "..."` streams to stdout and writes a session JSONL (verified live at M14).

---

# Milestone 6 — Sessions: discovery, hydration, continue/resume, `/new`

## Task 6.1: `active_branch_title` + `hydrate_transcript`

**Files:** `session_store.rs` (title), `tui/view.rs` (`hydrate`), a shared `hydrate::hydrate_transcript`.

**Interfaces:** `TranscriptItem{User,Assistant,Reasoning,ToolCall,ToolResult,CompactionMarker}`; `hydrate_transcript(&Session)->Result<Vec<TranscriptItem>>` (walks head→root active branch, reverses); `active_branch_title(&Session)->String` (oldest user-text on the head chain); `InteractiveShell::hydrate(&mut self,&Session)`.

- [ ] **Step 1: Failing tests** (create a `Session`, append user/assistant/tool_result entries via public API; assert `hydrate_transcript` returns items in chronological active-branch order and excludes an abandoned branch after `checkout`; `active_branch_title` returns the first user text). Use `Session::create` in a tempdir + `session_mut().append(...)`.
- [ ] **Step 2: Run — fails. Step 3: Implement** per design §6.5/§10.3 (walk `session.head()` via `entry.parent`; map `EntryValue::Message`/`Compaction`; skip `Config`). `SessionStore::list` now fills `title` via `active_branch_title(&Session::open(path)?)` (open each; small N).
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): active-branch hydration + titles`.

## Task 6.2: `rebuild_app` consuming transition + `/new` + resume wiring

**Files:** `app/mod.rs` (`rebuild_app`), `modes/interactive.rs`, `commands.rs`.

**Interfaces:** `rebuild_app(App, Option<Model>, Option<ReasoningConfig>, Option<SessionSelection>)->Result<App>` (design §5.2); `Reconfig` enum (design §6.4); `apply_reconfig(App,Reconfig)->Result<App>`; `commands::parse(&str)->Command`.

- [ ] **Step 1: Failing test** (`app`): `rebuild_app` with `session=None` preserves head/entries (same session reopened); with `CreateNew` yields a fresh empty session; a provenance `EntryValue::Config` entry is appended. Assert via `app.agent.session().entries()`.
- [ ] **Step 2: Run — fails. Step 3: Implement** `rebuild_app` (design §5.2 — drop old agent first; open/create; append `Config{model,reasoning}`; rebuild). Add `Command` enum + `commands::parse` (`/new`,`/resume [id]`,`/model [id]`,`/thinking [lvl]`,`/theme [name]`,`/compact`,`/status`,`/help`,`/quit`, else `Unknown`). Wire idle-phase `/new` → `apply_reconfig(app, Reconfig::NewSession)` then `shell.hydrate(app.agent.session())`. Wire `--continue`/`--resume` at startup (interactive) via a new `resolve_launch_interactive` that, with no model, defers to the M7 picker (for M6, require `--model`).
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): consuming rebuild_app, /new, resume wiring`.

## Task 6.3: Session picker (`tui/pickers.rs`) + `/resume`

**Interfaces:** `pick_from(&mut InteractiveShell,&mut EventStream, items:Vec<SelectItem>)->Result<Option<usize>>` (owned `SelectList`, encoded keys, `selected_item()`); `session_picker(...)`.

- [ ] **Step 1: Implement** `pick_from`: build `SelectList::new(items, max_visible, select_list_theme(&theme))`; loop over `EventStream`: `Up/Down`→`sl.handle_input(&encode(k))`; `Enter`→return selected index; `Esc`→`None`; printable→update filter via `sl.set_filter`. Render `sl.render(width)` composited by the shell. `/resume` (no id) → `session_picker` over `app.sessions.list()`; with id → `apply_reconfig(app, Reconfig::Resume(path))` + hydrate.
- [ ] **Step 2: Build.** **Step 3:** unit-test the filter/selection state is not feasible headlessly; instead unit-test `session_items(&SessionStore)->Vec<SelectItem>` (id+title mapping). **Step 4: Commit** `feat(coding-agent): session picker + /resume`.

**Acceptance (M6):** hydration + title + rebuild tests pass; `/new` and `--resume <id>` work (live at M14); pickers build.

---

# Milestone 7 — `/model` (startup picker + runtime switch)

## Task 7.1: Model resolution order + interactive picker

**Files:** `app/bootstrap.rs` (`resolve_launch_interactive`), `tui/pickers.rs` (`model_picker`), `commands.rs`.

**Interfaces:** `resolve_launch_interactive(&Bootstrap,&mut InteractiveShell,&mut EventStream)->Result<LaunchSelection>` (design §5.2); `model_picker(&mut shell,&mut input,&ModelCatalog)->Result<ModelId>`.

- [ ] **Step 1: Failing test:** model resolution order — a helper `resolve_model_id(cli:Option<ModelId>, project:Option<ModelId>, global:Option<ModelId>) -> Option<ModelId>` returns CLI→project→global precedence; `None` when all absent (→ picker at runtime / error in print). Unit-test the precedence.
- [ ] **Step 2: Run — fails. Step 3: Implement** `resolve_model_id`; `resolve_launch_interactive` (if `config.model` none → `model_picker` over `catalog.models()`). `model_picker` = `pick_from` over model ids. Runtime `/model` in idle phase: no arg → `model_picker` → `apply_reconfig(app, Reconfig::Model(id))`; with arg → resolve + `apply_reconfig`. If invoked mid-run, enqueue a `pending_reconfig: Option<Reconfig>` applied after the run settles.
- [ ] **Step 2b:** unit-test `pending_reconfig` application ordering (a helper `take_pending(&mut Option<Reconfig>)`).
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): model resolution order + /model picker & runtime switch`.

**Acceptance (M7):** precedence + pending-reconfig tests pass; `/model` switches model, writes provenance, keeps transcript (live at M14); status bar shows the model.

---

# Milestone 8 — `/thinking` (capability-aware)

## Task 8.1: `ThinkingLevel` + `thinking_to_reasoning` + `/thinking`

**Files:** `config.rs` (`ThinkingLevel`), `app/mod.rs` (`thinking_to_reasoning`), `commands.rs`.

**Interfaces:** `ThinkingLevel{Off,Minimal,Low,Medium,High}` with `to_effort()->ReasoningEffort`, `pick_budget(&ReasoningEffortBudgets)->u64`; `thinking_to_reasoning(ThinkingLevel,&Model)->Result<ReasoningConfig>`; `supported_levels(&Model)->Vec<ThinkingLevel>`.

- [ ] **Step 1: Failing tests:** `thinking_to_reasoning` — Effort model maps `High`→`Effort(High)`; TokenBudget model maps `High`→`Budget(budgets.high)`; `None`-capability model: `Off`→Ok(Off), any other→Err; `supported_levels` returns `[Off]` for no-capability, full set otherwise. Build a scripted `Model` with each capability (reuse the pattern from `agent_run.rs` frozen tests: construct `ModelSpec` with `Capabilities{ reasoning: Some(ReasoningCapability{control, effort_budgets, ..}) }`).
- [ ] **Step 2: Run — fails. Step 3: Implement** per design §6.4 (`thinking_to_reasoning`, `supported_levels`, `ThinkingLevel` conversions). Runtime `/thinking`: no arg → level picker over `supported_levels(&app.model)`; with level → `thinking_to_reasoning(level,&app.model)?` → `apply_reconfig(app, Reconfig::Thinking(rc))`. Status bar shows `reasoning_label(&app.reasoning)`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): /thinking capability-aware + runtime switch`.

**Acceptance (M8):** mapping tests pass; `/thinking` offers only supported levels, switches via consuming transition, status shows the level.

---

# Milestone 9 — Compaction (automatic + `/compact`)

## Task 9.1: Estimator, budgets, boundary, gate (`compaction.rs`)

**Interfaces (design §9):** `estimate_next_request_tokens(&App,&str)`, `estimate_messages_tokens`, `hard_input_budget(&Model)`, `soft_threshold(&Model,&CompactionPolicy)`, `choose_first_kept(&Session,usize)->Option<EntryId>`, `messages_before(&Session,&EntryId)->Result<Vec<Message>>`, `summarize(&AiClient,&Model,&[Message])`, `attempt_compaction(&mut App)->Result<CompactionOutcome>`, `ensure_capacity_before_prompt(&mut App,&str)->Result<CapacityDecision>`; `CompactionOutcome`, `CapacityDecision`.

- [ ] **Step 1: Failing tests (boundary invariants §9.3):** build sessions and assert — (1) `choose_first_kept` returns an **assistant** entry; (2) the entry before it is a completed user turn; (3) after `session.compact(summary,first_kept)`, `session.context()` alternates user/assistant with no orphaned tool-result; (4) single-huge-turn → `None`; (5) checkout-then-compact selects on the active branch. Also test `estimate_next_request_tokens` monotonic in prompt length, and `should_compact`/budgets math (occupancy vs `context_window - max_output_tokens`).
- [ ] **Step 2: Run — fails. Step 3: Implement** exactly per design §9 (`estimate_*`, `hard_input_budget`/`soft_threshold`, `choose_first_kept` assistant-boundary algorithm, `attempt_compaction`, `ensure_capacity_before_prompt` with soft-attempt + hard-recheck → `CapacityDecision::Exceeded`). `summarize` builds a `Request` with `tool_choice: ToolChoice::None`, `reasoning: Off`, and calls `app.client.complete`.
- [ ] **Step 4: Pass.** **Step 5: Wire the gate** into the interactive idle phase (design §6.2: `Exceeded`→error notice + `continue`; `Proceed`→report) and into `run_print` (M5 marker → `Exceeded`→`bail!`). Add `/compact` → force `attempt_compaction` at idle, report outcome.
- [ ] **Step 6: fmt+clippy+test. Step 7: Commit** `feat(coding-agent): pre-request compaction gate + /compact`.

**Acceptance (M9):** boundary + budget tests pass; auto-compaction triggers below the hard budget; a too-large prompt is refused with an actionable error; `/compact` reports outcome.

---

# Milestone 10 — `AGENTS.md` + layered configuration

## Task 10.1: `compose_instructions` (`resources.rs`)

**Interfaces:** `compose_instructions(&Config)->Result<String>`; `dirs_from_workspace_to_cwd(&Path,&Path)->Vec<PathBuf>`.

- [ ] **Step 1: Failing tests:** given `~/.ygg/AGENTS.md`-style + workspace-root + nested dirs, composition is `global → root → …leaf` in order (assert substring order); discovery never ascends above workspace (a parent-of-workspace `AGENTS.md` is excluded); `dirs_from_workspace_to_cwd` yields workspace first, cwd last. Use tempdirs; inject the global path via a param for testability.
- [ ] **Step 2: Run — fails. Step 3: Implement** per design §8.3 (ordered concat, base persona first). Wire into `resolve_launch_*`/`run_interactive`/`run_print` (replace the M4/M5 placeholder system string).
- [ ] **Step 4: Pass. Step 5: Layered config:** add `config.toml` global+project layers to `build_config` (merge order defaults→global→project→env→CLI); unit-test precedence (CLI overrides project overrides global). **Step 6: Commit** `feat(coding-agent): AGENTS.md composition + layered config`.

**Acceptance (M10):** composition + precedence tests pass; system prompt reflects `AGENTS.md`.

---

# Milestone 11 — Themes: loading, picker, `/theme`

## Task 11.1: Theme discovery + `/theme`

**Files:** `tui/theme.rs`, `commands.rs`, `tui/pickers.rs`.

**Interfaces:** `theme_path(name:&str,&Config)->Option<PathBuf>` (project over global); `available_themes(&Config)->Vec<String>`; `InteractiveShell::set_theme(&mut self, Theme)`.

- [ ] **Step 1: Failing tests:** `theme_path` resolves project over global; `available_themes` lists `.toml` stems from both dirs deduped (project wins). Tempdirs.
- [ ] **Step 2: Run — fails. Step 3: Implement.** `/theme` no arg → picker over `available_themes` → `shell.set_theme(Theme::load(path, tier))` (TUI-only, **no Agent rebuild**); with name → apply now. A missing/broken theme → notice, keep current (never fatal, never touches print). Rebuild the shell's cached `markdown_theme`/`select_list_theme` from the new `Theme`.
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): theme discovery, picker, /theme (TUI-only)`.

**Acceptance (M11):** theme tests pass; `/theme` switches colors live without rebuilding the Agent; a bad theme is a non-fatal notice.

---

# Milestone 12 — `/status`, `/help`, `/quit`, notices

## Task 12.1: Status, help overlay, quit, persistent errors

**Files:** `commands.rs`, `tui/view.rs`.

**Interfaces:** `status_text(&App, queued:Option<&Reconfig>)->String`; `help_text()->String`; `InteractiveShell::{show_overlay_text(&mut self,String), error(&mut self,String)}`; `/quit` handler.

- [ ] **Step 1: Failing test:** `status_text` includes model id, thinking label, workspace, session id/title, context estimate + hard budget, the security block (`Security model: trusted local agent`, `Workspace path guard (built-in tools): enabled`, `File edits`, `Process execution`, `Shell execution` as enabled/disabled, `OS isolation: none`, `Process privileges: current user`, `Repository trust: user-managed`), and queued-reconfig state. Assert each substring. The security block must never imply enabled process/shell execution is confined to the workspace. `help_text` must reference the concrete key bindings from M3.2 (Enter-idle for submit, Enter-active for follow-up, Ctrl+S for steer, Ctrl+C for abort, PageUp/PageDown for scroll, Esc to close overlays, Alt+Enter for newline) — never promise bindings that earlier milestones do not implement.
- [ ] **Step 2: Run — fails. Step 3: Implement.** `/status` → `show_overlay_text(status_text(...))`; `/help` → `show_overlay_text(help_text())`; `/quit` → if a run is active, `control.abort()` then drain to `RunFinished`; `shell.leave()` (restore); exit. `error(msg)` renders a visible persistent error line until dismissed. Overlays close on any key (Esc or any printable).
- [ ] **Step 4: Pass. Step 5: Commit** `feat(coding-agent): /status, /help, /quit, persistent errors`.

**Acceptance (M12):** status/help text tests pass; `/quit` restores the terminal and exits 0; errors are visible and persistent.

---

# Milestone 13 — Full regression tests

## Task 13.1: Cross-cutting tests + scripted event e2e

- [ ] **Step 1:** Add a scripted `AgentEvent` end-to-end test for the shell mapping: feed a hand-built sequence (`OutputDelta{Reasoning}`, `OutputDelta{Text}`, `ToolStarted`, `ToolProgress`, `ToolFinished`, `TurnFinished`, `RunFinished`) into `InteractiveShell::on_agent_event` and assert the transcript/gauge/panel state (pure, no TTY).
- [ ] **Step 2:** Add an interactive-loop integration test against the frozen wiremock scripted-model pattern (reuse `agent_run.rs`'s approach: a `Model` pointing at a wiremock server) driving `drive_active_run` through a full run to `RunEnded::Completed`. Use a channel-backed input source — create a `tokio::sync::mpsc::channel` and wrap the receiver with `tokio_stream::wrappers::ReceiverStream`, which satisfies the `S: Stream<Item = Result<Event>> + Unpin` bound on `drive_active_run`. Verify: steering enqueues in order; a finished run drains intents; an active `/model` command queues a `PendingIdleAction` rather than forwarding to the model.
- [ ] **Step 3:** Run the entire suite + frozen crates + clippy + fmt:
```bash
cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features 2>&1 | grep "test result" | tail
```
Expected: all green; all existing ygg-ai and ygg-agent tests remain green; no frozen test is removed, ignored, or weakened.
- [ ] **Step 4: Commit** `test(coding-agent): full regression + scripted event e2e`.

**Acceptance (M13):** whole workspace green (all existing frozen ygg-ai + ygg-agent tests unchanged, plus coding-agent units + e2e); clippy/fmt clean. Report actual frozen test count at gate time.

---

# Milestone 14 — Real-TTY / live-provider acceptance gate (MANUAL)

> Human-run, needs a real terminal + a provider API key. The agent will pause here — complete these steps manually, then resume.

- [ ] **Step 1: Interactive daily-use pass** (set provider API credentials per the resolved endpoint's auth requirements; `cargo run -p ygg-coding-agent --bin ygg` in a git workspace, selecting a reasoning-capable model from the embedded `ModelCatalog`). Verify the full loop: hydrated empty transcript; multiline editor; submit; streamed thinking (distinct) + text; a real `edit`+`read`+`exec` tool run with live output; steer mid-run (Ctrl+S); queue a follow-up (Enter during active); abort with Ctrl+C (clean); `/model` switch; `/thinking` change (status updates); `/theme` change; `/status`; `/help`; `/compact`; `/new`; `/resume`; `/quit` (terminal restored, exit 0). Resume the session in a new launch (`--continue`) and confirm the transcript hydrates.
- [ ] **Step 2: Print pass** (`ygg --print "create hello.txt with 'ygg' then read it"` with a configured model from the embedded `ModelCatalog`): streamed stdout; file created; session JSONL persisted under `~/.ygg/sessions/<key>/`; exit 0; missing-model error is actionable.
- [ ] **Step 3: Verify active-slash commands during a live run:** type `/status` and press Enter while the model is streaming — the status overlay appears immediately without being sent to the model. Type `/model` and press Enter during a run — the model change is queued and applied after the run settles. Type `/quit` during a run — the run is aborted, the terminal is restored, and the process exits cleanly.
- [ ] **Step 3: Record acceptance** (which items passed) and file any defects as follow-ups.

**Acceptance (M14):** the agent is genuinely usable end to end as a daily coding agent; terminal always restored; sessions persist and resume; active-slash commands work during a live run without model interference.

---

## Self-Review

**1. Spec coverage.** Every v1 capability in design §2 maps to a milestone: TUI shell/streaming/tools/status (M3), steer/queue/abort (M4), print (M5), sessions/hydration/continue/resume/`/new` (M6), `/model` (M7), `/thinking` (M8), compaction+`/compact` (M9), AGENTS.md+config (M10), themes+`/theme` (M11), `/status`/`/help`/`/quit`/errors (M12), regression (M13), acceptance (M14). Gate 0 (M1) precedes all. Removed `/checkout`/`/prompt` appear nowhere as commands. No named-prompt subsystem.

**2. Placeholder scan.** No "TODO/handle appropriately". Two explicit forward-links are named, not placeholders: the M5→M9 capacity-gate insertion point, and M6's `--model`-required-until-M7 interactive model resolution. Both are real, ordered dependencies.

**3. Type consistency.** `RunEnded`/`From<FinishReason>` defined once in `modes/mod.rs` (M4/M5), reused everywhere. `App` field set (M3.3) matches design §5.2 and is consumed identically by `rebuild_app` (M6.2), `/model` (M7), `/thinking` (M8), compaction (M9), status (M12). `Reconfig`/`apply_reconfig`/`rebuild_app` signatures match design §5.2/§6.4. `InputAction`/`EditAction`/`encode` (M3.2) are used by the loop (M4) and pickers (M6.3). `CapacityDecision`/`CompactionOutcome` (M9) match design §9.4. `SessionSelection`, `LaunchSelection`, `Bootstrap` match §5.2. `ThinkingLevel`/`thinking_to_reasoning` (M8) match §6.4.

**Integration risks (compile-caught, localized):** exact `sexy_tui::Terminal` trait method set, `MarkdownTheme`/`SelectListTheme` field lists, and `SelectList` filter/nav strings on the rust-port branch (M1.2/M3.1/M3.4/M6.3 builds catch these); the `in_flight` `select!` borrow shape (M4). All are compile-time and do not change the architecture.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-12-ygg-coding-agent-v1.md`. It **supersedes** the slice-1 plan and runs from Gate 0 through a complete daily-usable interactive v1. Post-v1 items: `docs/superpowers/backlog/ygg-coding-agent-post-v1.md`.

**Manual, human-run gates:** M1 Task 1.2 Step 7 (spike, real TTY) and M14 (acceptance, TTY + live provider). The agent will pause at these two gates — complete the manual verification steps in a real terminal, then resume.

**Execution:** Single-agent, inline. Work through tasks sequentially, committing at each task boundary. Pause at the two manual gates for human verification before continuing.
