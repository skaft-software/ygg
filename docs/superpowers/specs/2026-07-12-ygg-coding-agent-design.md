# `ygg-coding-agent` — v1 Design Specification

**Date:** 2026-07-12
**Status:** Approved design (Revision 3 — TUI-primary realignment)
**Frozen dependencies:** `crates/ygg-ai`, `crates/ygg-agent` (see *Preconditions*)

## Primary product

**`ygg-coding-agent` is a full-screen interactive coding agent built with
`sexy-tui-rs`, launched by the `ygg` executable.** The command line is only the
launcher and startup-configuration surface; it opens the TUI. Inside the TUI the
user drives the app with slash commands (`/model`, `/thinking`, `/theme`,
`/compact`, `/new`, `/resume`, `/status`, `/help`, `/quit`). **Print mode
(`--print`) is a secondary, headless frontend over the same runtime.** v1 is the
complete daily-usable interactive product (§2), not a line-oriented CLI agent and
not a single fresh-session "slice".

```text
ygg                          → open the interactive TUI
ygg "fix the tests"          → open the TUI and immediately submit the prompt
ygg --continue               → open the TUI with the latest workspace session hydrated
ygg --resume [id]            → open the TUI with a selected session hydrated
ygg --print "explain repo"   → secondary noninteractive/headless mode
```

> Revision 2 applied seven architectural corrections: explicit idle/active loop
> phases (no permanent `Option<Run>`); a bootstrap/launch-selection phase split
> from `Agent` construction with `Theme` removed from the mode-agnostic `App`;
> compaction as a pre-request capacity gate with verified `Usage` semantics and a
> semantic turn-boundary algorithm; exact workspace + `AGENTS.md` scope; resume
> transcript hydration on the active branch; persistent print sessions; and
> smaller correctness fixes.
>
> Revision 2.1 (narrow correctness pass): control sends are **queued** off the
> input arm so a full control channel cannot stall `run.next()` (§6.2); the
> compaction gate **drops `Usage` calibration** and estimates the next request
> directly (§9.2), adds a **hard-capacity `CapacityDecision::Exceeded`** failure
> path that refuses to prompt (§9.4); tool-schema reserve reads the **frozen
> `Tool::definition()`** source, not reproduced schemas (§5.2); print mode tracks
> `RunFinished` explicitly with per-reason exit semantics (§6.7); and the Gate-0
> spike handles **Ctrl+C as a raw-mode key event** (§12).
>
> Revision 3 (TUI-primary realignment): the **interactive TUI is the primary
> product** and v1 is the complete daily-usable product, not a fresh-session
> slice. The slash-command grammar is finalized to `/model /thinking /theme
> /compact /new /resume /status /help /quit`; **`/checkout` and `/prompt` are
> removed** from v1 (checkout stays an internal primitive; named prompts/skills
> are deferred, §8.4). **Runtime `/model`, `/thinking`, `/new`, `/resume`** are
> promoted into v1 via a safe **consuming `Agent` transition** (§6.4); `App`
> retains `catalog` + `sessions` to serve them (§5.2). sexy-tui integration
> facts are corrected against the rust-port source: `MarkdownTheme`/
> `SelectListTheme` have **no `Default`** and are built via closure helpers
> (§5.5); pickers drive `SelectList::handle_input(&str)` with a **replicated
> key encoder** and read `selected_item()` (§5.5/§7); terminal size is shared
> with the boxed `YggTerminal` via `Rc<Cell<(u16,u16)>>` for resize (§5.4/§7).
> Verified frozen test count: **222**. The full v1 implementation plan is
> `docs/superpowers/plans/2026-07-12-ygg-coding-agent-v1.md`.
>
> Revision 4 (native multimodal input, 2026-07-15): the ygg-agent boundary was
> widened — `prompt`, `steer`, `follow_up` now accept `impl Into<UserInput>`
> (was `impl Into<String>`) with ordered `InputPart::Text`/`InputPart::Media`
> parts; text-only callers remain source-compatible. The TUI composer gained
> an attachment ledger with chips for media-path pastes/drops, large-paste
> collapsing, gitignore-aware `@` file-mention completion, and capability-gated
> attach-time validation. `ygg-ai` is unchanged — its `UserPart::Media` already
> supported both modalities. Full design: `docs/superpowers/specs/2026-07-15-multimodal-input-design.md`;
> implementation plan: `docs/superpowers/plans/2026-07-15-multimodal-input.md`.

---

## Preconditions and scope guards

This design assumes two already-merged, narrowly-scoped changes to `ygg-agent`:
1. `AgentConfig` carries `pub reasoning: ReasoningConfig`, and `Agent::prompt`
   threads it into every `ygg_ai::Request` instead of hardcoding
   `ReasoningConfig::Off`. Streaming reasoning is therefore configurable
   through the frozen public API.
2. The agent boundary accepts `impl Into<UserInput>` (ordered `InputPart::Text` /
   `InputPart::Media` parts) on `prompt`, `steer`, and `follow_up`. Text-only
   callers remain source-compatible via `From<String>` / `From<&str>`.
   `ygg-ai` is unchanged — its `UserPart::Media` already supports both
   modalities. Full design:
   `docs/superpowers/specs/2026-07-15-multimodal-input-design.md`.

**`ygg-coding-agent` must not re-implement or work around any of these**;
it only *sets* `AgentConfig.reasoning` at construction and passes
`UserInput`/`ComposedInput` at submit, steer, and follow-up. With those
fields in place `ygg-agent` is refrozen.

Everything else in `ygg-ai` / `ygg-agent` is used exactly as published. This
crate duplicates none of their responsibilities: the one mutable session head,
sequential tool execution, durable append-only JSONL, branching/checkout,
compaction *markers*, streaming, bounded tool progress, immediate result
persistence, steering/follow-up/abort, extension tools/observers, and the
workspace-path-guarded tools all remain theirs.

The TUI is built on **`sexy-tui-rs`** (the `rust-port` branch of
`github.com/achuthanmukundan00/sexy-tui-rs`): a Rust port of pi-tui with a
retained `Component` tree, differential rendering, overlays, a 3-layer TOML
theme engine, and `Markdown`/`Editor`/`SelectList`/`Loader` widgets.

---

## 1. Executive architecture verdict

**`ygg-coding-agent` is a procedural product shell around the frozen
`ygg_agent::Agent`. There is no central `CodingAgent` or `CodingSession`
object.**

The frozen `Agent` already *is* the stateful core the brief warns against
duplicating: it owns the single mutable `Session` head, the `Model`, the tool
registry (`ExtensionHost`), the `SandboxConfig`, the system prompt, and the run
loop. A `Run<'a>` borrows `&mut Agent` for the lifetime of one run and exposes a
clonable `RunControl` (`agent.rs:107,139–172`). A second orchestration object on
top of that would be a god object by construction.

The product is a small mode-agnostic `App` (model/session state), an
`InteractiveShell` (all terminal I/O, interactive only), and free functions. The
interactive experience runs on the **main thread** with **explicit idle and
active phases** (§6.2): between runs the code freely borrows `&mut App`; while a
`Run` is alive (borrowing `&mut app.agent`) the code touches only the run, its
control handle, and the shell — never `App`/`Agent`. This respects `Run<'a>`'s
borrow and keeps every stateful concern singly owned.

**How this keeps Terminus simplicity while delivering the Pi behaviors that
matter:** control flow is procedural and readable top-to-bottom (Terminus). The
Pi behaviors — streaming text + reasoning, live tool output, steering, queued
follow-ups, abort, resume/branch, compaction, project instructions, themes,
model picking — are delivered by *composing* the frozen `Agent` event stream
with sexy-tui widgets, not by a resource/service framework. No event bus, no
dependency injection, no dynamic plugin ABI.

---

## 2. Exact v1 scope

v1 is the **complete daily-usable interactive coding agent**. The full-screen
TUI is the primary product; print mode is a secondary headless frontend over the
same runtime.

### Required (in v1) — interactive TUI (primary)

| # | Capability | Delivered by |
|---|------------|--------------|
| 1 | Full-screen `sexy-tui-rs` interface; hydrated, scrollable transcript | `tui/view.rs` (`InteractiveShell`) |
| 2 | Multiline editor; submit/steer/follow-up/abort bindings | `tui/keymap.rs` + `RunControl` |
| 3 | Streaming assistant text with **separate thinking presentation** | `AgentEvent::OutputDelta{channel}` → distinct blocks |
| 4 | Live tool start/progress/finish panels; **bounded display memory** | `ToolStarted/ToolProgress/ToolFinished` |
| 5 | Status bar (model, thinking, workspace, session, context gauge) | `tui/view.rs` |
| 6 | Steer, ordered queued follow-ups, synchronous abort | queued `ControlIntent` (§6.2) |
| 7 | `/model` picker + runtime switch (consuming transition) | `tui/pickers.rs`, §6.4 |
| 8 | `/thinking` picker + runtime switch (capability-aware) | §6.4 |
| 9 | `/theme` picker + live reload (TUI-only) | `tui/theme.rs`, §8.4 |
| 10 | `/compact` manual + automatic pre-request compaction | §9 |
| 11 | `/new` (fresh session) + `/resume` picker/by-id (consuming transition) | §6.4/§10 |
| 12 | `/status`, `/help` overlay, `/quit` (abort + restore + exit) | §8.4 |
| 13 | Session persistence, discovery, continue, resume; active-branch hydration | frozen `Session`, `SessionStore`, §6.5/§10 |
| 14 | Startup + runtime model/thinking resolution (capability-aware) | §8.2/§6.4 |
| 15 | `AGENTS.md` workspace-scoped project instructions | §8.3 |
| 16 | Visible persistent errors; resize; safe terminal restoration | §5.4/§7 |

### Required (in v1) — print mode (secondary) & shared runtime

| # | Capability | Delivered by |
|---|------------|--------------|
| 17 | One procedural runtime over the frozen `Agent`; no 2nd loop / event bus / actors / `Arc<Mutex>` | §1, §6.2 |
| 18 | Persistent print session; explicit `RunFinished` classification; optional reasoning; actionable unresolved-model error; no TUI/theme init | `modes/print.rs`, §6.7 |
| 19 | One current-thread Tokio runtime for both frontends | §7 |

**Command grammar (v1, in-TUI slash commands):**
`/model [id]`, `/thinking [off|minimal|low|medium|high]`, `/theme [name]`,
`/compact`, `/new`, `/resume [id]`, `/status`, `/help`, `/quit`. Anything not
beginning with `/` is a prompt (§8.4). These are **TUI commands, not shell CLI
commands**.

### Explicitly removed from v1 (see backlog)

- **OAuth / subscription login (OpenAI Codex, Anthropic Pro/Max, GitHub
  Copilot).** The `Auth::Dynamic` + `CredentialResolver` infrastructure is
  implemented in `ygg-ai` and ready, but no concrete resolver exists yet. The
  catalog has no Codex/subscription endpoints, and the bootstrap code does not
  register an OAuth resolver. This is a deferred feature tracked in
  `docs/plans/ygg-ai-oauth-codex.md`. The PI reference
  (`docs/research/refImpls/ai/pi-ai.md` §"Auth Subsystem") documents the full
  OAuth pattern.
- **`/checkout`** and **`/prompt`** are **not** v1 commands. `Session::checkout`
  remains an internal foundation primitive, but exposing raw entry IDs is not
  acceptable daily UX; a branch/fork UI over visible transcript messages is
  post-v1. Named prompts / skills and any resource subsystem for them are
  deferred — **`AGENTS.md` is sufficient project-instruction support for v1**.
- Manual session rename/search/archive/export; session-tree view; approval
  dialogs; rich diff viewer; image rendering; reactive overflow-retry compaction;
  retries/backoff; and everything in the brief's non-goals list. See
  `docs/superpowers/backlog/ygg-coding-agent-post-v1.md`.

### Approximate implementation surface

New crate `crates/ygg-coding-agent` (binary `ygg`), ~4,000–5,500 LOC across the
module tree in §4. No changes to frozen crates beyond the merged reasoning field.

---

## 3. Responsibility table

| Responsibility | Owner |
|----------------|-------|
| Provider HTTP, protocol codecs, streaming assembly | `ygg-ai` |
| Model catalog, resolution, capabilities, pricing | `ygg-ai` (`ModelCatalog`) |
| Credential resolution (env/bearer/header/dynamic) | `ygg-ai` (`Auth`) |
| **Concrete OAuth resolver (Codex, Anthropic Pro/Max, Copilot)** | **deferred** — `Auth::Dynamic` trait ready; see `docs/plans/ygg-ai-oauth-codex.md` |
| Canonical request/response/**usage**/reasoning types | `ygg-ai` |
| Agent run loop, turn/tool sequencing | `ygg-agent` (`Agent::prompt`) |
| One mutable session head; append/checkout/compact/context | `ygg-agent` (`Session`) |
| Durable JSONL persistence & crash recovery | `ygg-agent` (`Session`) |
| Tool execution, sandbox path checks, bounded progress + **bounded final result** | `ygg-agent` (tools/sandbox) |
| Steering / follow-up / abort mechanics | `ygg-agent` (`RunControl`) |
| Tool/observer registration boundary | `ygg-agent` (`ExtensionHost`) |
| Reasoning value applied to requests | `ygg-agent` (`AgentConfig.reasoning`); *set by* `ygg-coding-agent` |
| CLI parsing & mode dispatch | `ygg-coding-agent` (`cli`, `main`) |
| Config layering & precedence | `ygg-coding-agent` (`config`) |
| **Workspace resolution** (`.git` ancestor) | `ygg-coding-agent` (`config`) |
| **Bootstrap + launch selection** (model/session) | `ygg-coding-agent` (`app::bootstrap`) |
| Session directory layout, discovery, **active-branch titles** | `ygg-coding-agent` (`session_store`) |
| `AGENTS.md` discovery (workspace-scoped) → system prompt | `ygg-coding-agent` (`resources`) |
| **Runtime reconfiguration** (`/model`,`/thinking`,`/new`,`/resume` via consuming rebuild) | `ygg-coding-agent` (`app::rebuild_app`, `modes/interactive`) |
| Pickers (model/thinking/theme/session) | `ygg-coding-agent` (`tui/pickers`) + `sexy-tui-rs` (`SelectList`) |
| **Compaction policy** (pre-request gate, boundary, summarize) | `ygg-coding-agent` (`compaction`) |
| Interactive idle/active loop, key handling, steering wiring | `ygg-coding-agent` (`modes/interactive`) |
| **Resume transcript hydration** | `ygg-coding-agent` (`tui/view` + `session_store`) |
| Print-mode streaming to stdout | `ygg-coding-agent` (`modes/print`) |
| Terminal raw/alt-screen, **idempotent chained restore**, render primitives | `ygg-coding-agent` (`tui/terminal` — `YggTerminal`) |
| Transcript/status/input view state, `AgentEvent`→widget | `ygg-coding-agent` (`tui/view` — `InteractiveShell`) |
| **Theme** (path resolution, reload) — TUI only, never in `App` | `ygg-coding-agent` (`tui/theme`) + `sexy-tui-rs` (`Theme`) |
| Model/session pickers (overlays) | `ygg-coding-agent` (`tui/pickers`) |
| Retained tree, differential paint, overlays, widgets, theme resolution | `sexy-tui-rs` |
| Core tools (`read`/`search`/`edit`/`exec`) | `ygg-agent` (`CoreTools` extension) |

---

## 4. Module tree

```
crates/ygg-coding-agent/
  Cargo.toml               # bin `ygg`; deps: ygg-agent, ygg-ai, sexy-tui-rs (path),
                           #   tokio (rt, macros, time, io-util, process — current_thread),
                           #   crossterm (event-stream), futures-util, clap, serde, toml, dirs, anyhow
  src/
    main.rs                # #[tokio::main(flavor="current_thread")]; parse → dispatch
    cli.rs                 # Cli (clap) → config overrides
    config.rs              # Config, resolve() w/ precedence, workspace resolution (.git ancestor)
    app/
      mod.rs               # App (mode-agnostic: no Theme, no TUI)
      bootstrap.rs         # Bootstrap, LaunchSelection, resolve_launch_*(), build_app()
    session_store.rs       # SessionStore: layout, latest/by_id/list, active-branch title
    resources.rs           # AGENTS.md (workspace-scoped) composition; named prompt discovery
    compaction.rs          # estimate_next_request_tokens, budgets, choose_first_kept, ensure_capacity_before_prompt
    modes/
      mod.rs
      interactive.rs       # run_interactive: idle/active phases (§6.2)
      print.rs             # run_print: headless stream → stdout (persistent session)
    tui/
      mod.rs
      terminal.rs          # YggTerminal: impl sexy_tui::Terminal (render + dims + idempotent restore)
      view.rs              # InteractiveShell: TUI + Theme + Editor + transcript + status; hydrate()
      theme.rs             # theme path resolution + /theme reload
      pickers.rs           # model_picker(), session_picker() → SelectList overlays
      keymap.rs            # crossterm Event → InputAction (pure translation)
    spike/
      bin_spike.rs         # Gate-0 TUI integration spike (§12 gate 0)
```

---

## 5. Core types and APIs

All sketches are written to be borrow- and type-realistic (compile-oriented
self-review in §12 preface). Ownership constraints are called out inline.

### 5.1 Configuration and workspace

```rust
/// One resolved, immutable-per-process configuration. No per-module configs.
pub struct Config {
    pub workspace: PathBuf,           // canonicalized (§8.0)
    pub invocation_cwd: PathBuf,      // canonicalized; must be within workspace
    pub model: Option<ModelId>,       // None ⇒ resolve interactively (or error in print)
    pub reasoning: ReasoningConfig,   // ygg-ai canonical; default Off
    pub sandbox: SandboxPolicy,       // maps to ygg_agent::SandboxConfig
    pub theme: Option<String>,        // theme name; TUI-only (never affects print)
    pub session_dir: PathBuf,         // default ~/.ygg/sessions
    pub compaction: CompactionPolicy, // §9
    pub max_turns: u64,               // default 40
    pub show_reasoning_in_print: bool,// default false
    pub mode: Mode,                   // Interactive | Print { prompt: String }
    pub resume: ResumeSelector,       // New | Continue | Resume(Option<SessionId>)
}

pub enum Mode { Interactive, Print { prompt: String } }

pub struct SandboxPolicy {            // product-level → SandboxConfig fields
    pub allow_edit: bool,             // default true
    pub allow_process: bool,          // default true
    pub allow_shell: bool,            // default false (opt-in)
    pub exec_timeout_secs: u64,       // default 120
    pub max_output_bytes: usize,      // default 64 KiB — bounds the FINAL tool result
}

pub struct CompactionPolicy {
    pub threshold_fraction: f64,      // default 0.85 of the input budget
    pub keep_recent_turns: usize,     // default 4
}
```

**Workspace resolution (§8.0).** `--workspace` if given, else the nearest
ancestor of cwd containing `.git`, else cwd; then `canonicalize()`. `AGENTS.md`
discovery is bounded to this workspace (§8.3).

### 5.2 Bootstrap → LaunchSelection → App (correction 2)

Bootstrap performs no `Agent` construction and no TUI. Model/session selection
is a separate phase. Only after selection is complete is the `Session` opened
and the `Agent` built. **`Theme` is not part of `App`** — a malformed/missing
theme can never break print mode.

```rust
/// Everything needed to *decide* what to launch. No Agent, no TUI, no Theme.
pub struct Bootstrap {
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub client: AiClient,
}

/// The resolved decision.
pub struct LaunchSelection {
    pub model: ModelId,
    pub session: SessionSelection,          // OpenExisting(PathBuf) | CreateNew(PathBuf)
}

pub enum SessionSelection { OpenExisting(PathBuf), CreateNew(PathBuf) }

pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap> {
    let catalog  = ModelCatalog::builtin()?;                    // ygg-ai embedded snapshot
    let sessions = SessionStore::new(&config.session_dir, &config.workspace);
    let client   = AiClient::new();
    Ok(Bootstrap { config, catalog, sessions, client })
}

/// Interactive resolution MAY open a lightweight picker overlay via `shell`.
/// `input` is threaded in (the shell does not own the EventStream — see §7).
pub async fn resolve_launch_interactive(
    boot: &Bootstrap,
    shell: &mut InteractiveShell,
    input: &mut crossterm::event::EventStream,
) -> anyhow::Result<LaunchSelection> {
    let model = match &boot.config.model {
        Some(id) => id.clone(),
        None => shell.pick_model(&boot.catalog, input).await?,     // SelectList overlay
    };
    let session = match &boot.config.resume {
        ResumeSelector::New              => SessionSelection::CreateNew(boot.sessions.new_path()),
        ResumeSelector::Continue         => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => SessionSelection::OpenExisting(boot.sessions.by_id(id)?.path),
        ResumeSelector::Resume(None)     => SessionSelection::OpenExisting(shell.pick_session(&boot.sessions, input).await?),
    };
    Ok(LaunchSelection { model, session })
}

/// Print resolution NEVER opens a picker; unresolved ⇒ actionable error.
pub fn resolve_launch_print(boot: &Bootstrap) -> anyhow::Result<LaunchSelection> {
    let model = boot.config.model.clone().ok_or_else(|| anyhow::anyhow!(
        "no model configured: pass --model <id> or set model in .ygg/config.toml (available: {})",
        boot.catalog.models().map(|m| m.id.0.as_str()).collect::<Vec<_>>().join(", ")
    ))?;
    let session = match &boot.config.resume {
        ResumeSelector::New              => SessionSelection::CreateNew(boot.sessions.new_path()),
        ResumeSelector::Continue         => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => SessionSelection::OpenExisting(boot.sessions.by_id(id)?.path),
        ResumeSelector::Resume(None)     => anyhow::bail!("--resume needs a session id in print mode"),
    };
    Ok(LaunchSelection { model, session })
}

/// Mode-agnostic owning struct. No Theme, no TUI.
pub struct App {
    pub agent: Agent,             // owns session/model/tools/reasoning/system
    pub model: Model,             // resolved binding; kept for compaction summarize()
    pub client: AiClient,         // coding-agent's own clone (compaction summaries)
    pub config: Config,
    pub catalog: ModelCatalog,    // retained for runtime /model + /thinking capability lookup (Clone)
    pub sessions: SessionStore,   // retained for runtime /new + /resume
    pub reasoning: ReasoningConfig,// current runtime thinking level (mirrors AgentConfig.reasoning; shown in status)
    pub system: String,           // current composed system prompt (reused when rebuilding the Agent)
    pub system_tokens: u64,       // precomputed estimate of the system prompt (§9)
    pub tool_schema_tokens: u64,  // precomputed estimate of all tool defs (§9)
}

/// Build the Agent AFTER selection. `client` is cloned into the Agent and kept
/// in `App` for the stateless compaction summarizer.
pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App> {
    let Bootstrap { config, catalog, sessions, client } = boot;   // retain catalog + sessions
    let model = catalog.resolve(&launch.model)?;
    let reasoning = config.reasoning.clone();
    let session = match launch.session {
        SessionSelection::CreateNew(p)  => Session::create(p)?,
        SessionSelection::OpenExisting(p) => Session::open(p)?,
    };

    // Tool-schema reserve read from the FROZEN source of truth: the exact tool
    // structs `CoreTools` registers (tools/mod.rs), via the public `Tool::definition()`.
    use ygg_agent::{ReadTool, SearchTool, EditTool, ExecTool, Tool};
    let tool_defs: [ToolDef; 4] =
        [ReadTool.definition(), SearchTool.definition(), EditTool.definition(), ExecTool.definition()];
    let tool_schema_tokens = tool_defs.iter().map(estimate_tooldef_tokens).sum();

    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);   // registers exactly the four tools measured above

    let system_tokens = estimate_text_tokens(&system);
    let agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),                              // the merged field
    })?;
    Ok(App { agent, model, client, config, catalog, sessions, reasoning, system,
             system_tokens, tool_schema_tokens })
}
```

The **consuming `Agent` transition** used by `/model`, `/thinking`, `/new`, and
`/resume` (§6.4) reuses this same construction: it takes `App` by value, drops the
old `Agent` (closing the session `File`) before opening/creating the session, and
returns a fresh `App` — never two owners of one session.

```rust
/// One safe consuming transition. Runs only at an idle boundary (no live Run).
/// `session` = None keeps the current session (model/thinking switch); Some(sel)
/// switches sessions (/new, /resume). Writes a provenance Config marker.
pub fn rebuild_app(
    app: App,
    new_model: Option<Model>,
    new_reasoning: Option<ReasoningConfig>,
    session: Option<SessionSelection>,
) -> anyhow::Result<App> {
    let App { agent, model, client, config, catalog, sessions, reasoning, system,
              system_tokens, tool_schema_tokens } = app;
    let current_path = agent.session().path().to_owned();
    drop(agent);                                                  // destroy the old owner FIRST

    let model = new_model.unwrap_or(model);
    let reasoning = new_reasoning.unwrap_or(reasoning);
    let mut session = match session {
        Some(SessionSelection::CreateNew(p))   => { if let Some(d)=p.parent(){std::fs::create_dir_all(d)?;} Session::create(p)? }
        Some(SessionSelection::OpenExisting(p)) => Session::open(p)?,
        None                                    => Session::open(current_path)?, // same session, single owner
    };
    // Provenance: record the model/reasoning in effect from here forward.
    session.append(EntryValue::Config {
        model: Some(model.spec.id.0.clone()),
        reasoning: Some(reasoning_label(&reasoning)),
    })?;

    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let agent = Agent::new(AgentConfig {
        client: client.clone(), model: model.clone(), session, system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions, max_turns: config.max_turns, reasoning: reasoning.clone(),
    })?;
    Ok(App { agent, model, client, config, catalog, sessions, reasoning, system,
             system_tokens, tool_schema_tokens })
}
```

### 5.3 InteractiveShell (owns all terminal I/O incl. Theme)

```rust
/// Owns the sexy-tui TUI + terminal + theme + editor + transcript.
/// Constructed only in interactive mode; never in print mode. `!Send`; main thread.
pub struct InteractiveShell {
    tui: sexy_tui::TUI,           // holds YggTerminal + retained component tree
    theme: Theme,                 // sexy-tui theme (TUI-only)
    editor: Editor,               // multiline input (sexy-tui)
    transcript: Container,        // Markdown blocks + tool panels, active branch order
    status: StatusBar,            // model, workspace, run-state, token gauge
    // streaming cursors:
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    tool_panels: HashMap<ToolCallId, usize>,
    run_state: RunState,          // Idle | Streaming | ToolRunning{name} | Compacting | Aborted | Failed
}

impl InteractiveShell {
    pub fn enter(theme: Theme) -> anyhow::Result<Self>;  // YggTerminal::enter + TUI::new + TUI::start
    pub fn leave(self);                                  // TUI::stop; restore (also RAII on YggTerminal)

    // Selection overlays (bootstrap phase):
    pub async fn pick_model(&mut self, catalog: &ModelCatalog, input: &mut EventStream) -> anyhow::Result<ModelId>;
    pub async fn pick_session(&mut self, sessions: &SessionStore, input: &mut EventStream) -> anyhow::Result<PathBuf>;

    // Resume hydration (correction 5):
    pub fn hydrate(&mut self, session: &Session) -> anyhow::Result<()>;

    // Per-event rendering (active phase):
    pub fn on_agent_event(&mut self, ev: &AgentEvent);
    pub fn on_turn_finished(&mut self, usage: &Usage);
    pub fn apply_edit(&mut self, action: EditAction);
    pub fn set_size(&mut self, cols: u16, rows: u16);
    pub fn set_run_state(&mut self, s: RunState);
    pub fn report_compaction(&mut self, outcome: &CompactionOutcome);
    pub fn animate(&mut self);
    pub fn render(&mut self);                            // TUI::request_render
    pub fn drain_editor(&mut self) -> String;           // take + reset input text
}
```

Note `pick_*` take `&mut EventStream` explicitly (not owned by the shell), so the
input source stays a distinct borrow usable by the loop's `select!` (§7).

### 5.4 Terminal adapter — idempotent, chained restore (corrections 4, 7)

```rust
/// Implements sexy-tui's Terminal for RENDER ONLY. We never call the blocking
/// `Terminal::start()`; input is driven by the loop via crossterm::EventStream.
/// Dimensions live in a shared cell so the shell can update them on resize even
/// though the terminal is boxed inside the TUI (verified: TUI::do_render reads
/// columns()/rows() and does not re-query the OS). Single-threaded ⇒ Rc/Cell OK.
pub struct YggTerminal { out: std::io::Stdout, size: std::rc::Rc<std::cell::Cell<(u16, u16)>> }

impl sexy_tui::Terminal for YggTerminal {
    fn start(&mut self, _in: Box<dyn FnMut(&str)>, _rs: Box<dyn FnMut()>) {
        unreachable!("YggTerminal::start is never called; input is driven by the select! loop");
    }
    fn write(&mut self, data: &str) { use std::io::Write; let _ = self.out.write_all(data.as_bytes()); let _ = self.out.flush(); }
    fn columns(&self) -> u16 { self.size.get().0 }
    fn rows(&self) -> u16 { self.size.get().1 }
    // clear_*, move_by, hide/show_cursor, enter/leave_alternate_screen, set_title,
    // set_progress, drain_input, stop: thin crossterm/ANSI wrappers.
}

impl YggTerminal {
    /// enable raw + alt screen + hide cursor; query size; RAW_ACTIVE=true.
    /// Returns the terminal and a clone of the size cell for the shell to update.
    pub fn enter() -> anyhow::Result<(Self, std::rc::Rc<std::cell::Cell<(u16, u16)>>)>;
}

/// Global idempotent restore guard, so Drop and the panic hook cannot double-restore.
static RAW_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn force_restore() {
    use std::sync::atomic::Ordering;
    if RAW_ACTIVE.swap(false, Ordering::SeqCst) {       // idempotent: only the first caller restores
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out,
            crossterm::terminal::LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Install once at startup; CHAINS the previous hook (correction 7).
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { force_restore(); prev(info); }));
}

impl Drop for YggTerminal { fn drop(&mut self) { force_restore(); } } // also idempotent
```

`YggTerminal::enter` sets `RAW_ACTIVE = true` after enabling raw mode; every
restore path funnels through `force_restore`, which restores at most once.

---

## 6. End-to-end workflow traces

### 6.1 Startup

```
process starts
→ cli::parse(argv)
→ config::resolve(cli, env, files):
    → resolve workspace: --workspace | nearest .git ancestor | cwd  → canonicalize
    → build Config (mode, resume, model?, reasoning, sandbox, theme?, …)
→ install_panic_hook()
→ boot = bootstrap(config)                        // catalog, sessions, client — no Agent/TUI/Theme
→ match config.mode:
     Interactive → run_interactive(boot)
     Print{prompt} → run_print(boot, prompt)
```

### 6.2 Interactive prompt — explicit idle / active phases (correction 1)

`Run<'a>` borrows `&mut app.agent`, so the code cannot hold `&mut App` while a run
is alive. The loop separates an **idle phase** (full `&mut App` access: capacity
gate, checkout, prompt insertion) from an **active phase** (`drive_active_run`,
which sees only the run, control, shell, input, and ticker — never `App`).

```rust
pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    // Terminal comes up first so selection overlays can render.
    let theme = load_theme(&boot.config);                 // TUI-only; failure ⇒ default theme, never fatal
    let mut shell = InteractiveShell::enter(theme)?;
    let mut input  = crossterm::event::EventStream::new(); // distinct borrow (not owned by shell)
    let mut ticker = tokio::time::interval(Duration::from_millis(80));

    let launch = resolve_launch_interactive(&boot, &mut shell, &mut input).await?;
    let system = resources::compose_instructions(&boot.config)?;   // AGENTS.md (§8.3)
    let mut app = build_app(boot, launch, system)?;
    shell.hydrate(app.agent.session())?;                  // resume transcript (correction 5)

    loop {
        match wait_for_prompt(&mut shell, &mut input, &mut ticker).await? {
            Idle::Quit => break,
            Idle::Command(cmd) => { run_command(&mut app, &mut shell, cmd).await?; continue; }
            Idle::Submit(text) => {
                // Pre-request capacity gate (§9), between runs → full &mut App.
                shell.set_run_state(RunState::Compacting);
                match ensure_capacity_before_prompt(&mut app, &text).await? {
                    CapacityDecision::Exceeded { estimate, budget } => {
                        shell.error(format!(
                            "prompt too large: ~{estimate} tokens exceeds the {budget}-token budget \
                             even after compaction — shorten it or start a new session"));
                        continue;                            // do NOT call Agent::prompt (§9.4)
                    }
                    CapacityDecision::Proceed(outcome) => shell.report_compaction(&outcome),
                }

                let mut run = app.agent.prompt(text).await?; // borrows &mut app.agent for the run
                let control = run.control();
                let ended = drive_active_run(
                    &mut run, &control, &mut shell, &mut input, &mut ticker,
                ).await?;                                    // NO &mut App inside
                drop(run);                                   // releases the borrow
                shell.set_run_state(RunState::from(&ended));
            }
        }
    }
    shell.leave();
    Ok(())
}
```

`drive_active_run` is the fast pump. Its `select!` arms borrow **disjoint**
sources (`input`, `run`, `ticker`, `in_flight`); arm *bodies* borrow `shell`/
`control`/the intent queue sequentially. `App`/`Agent` are untouched. Control
sends are **queued** (Revision 2.1 correction 1): the input arm never awaits
`steer`/`follow_up` — a full control channel would otherwise stall `run.next()`
and deadlock the agent (it drains that channel at turn boundaries). Instead the
input arm enqueues an ordered `ControlIntent`, and a dedicated arm drives **one**
in-flight send at a time while input, `run`, and the ticker stay responsive:

```rust
pub enum RunEnded { Completed, Aborted, MaxTurns, Failed(String) }

impl From<FinishReason> for RunEnded {
    fn from(r: FinishReason) -> Self {
        match r {
            FinishReason::Completed    => RunEnded::Completed,
            FinishReason::Aborted      => RunEnded::Aborted,
            FinishReason::MaxTurns     => RunEnded::MaxTurns,
            FinishReason::Failed(e)    => RunEnded::Failed(e.to_string()),
        }
    }
}

/// Ordered control intents queued off the input arm so a full control channel
/// never blocks `run.next()` (Revision 2.1 correction 1).
enum ControlIntent { Steer(String), FollowUp(String) }

async fn drive_active_run(
    run: &mut Run<'_>,
    control: &RunControl,
    shell: &mut InteractiveShell,
    input: &mut crossterm::event::EventStream,
    ticker: &mut tokio::time::Interval,
) -> anyhow::Result<RunEnded> {
    use futures_util::StreamExt;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;

    let mut intents: VecDeque<ControlIntent> = VecDeque::new();
    // Exactly one in-flight send. It OWNS a clone of `control` (RunControl: Clone)
    // and the text, so the boxed future is self-contained ('static): no borrow of
    // loop state, and no detached task.
    let mut in_flight: Option<Pin<Box<dyn Future<Output = Result<(), AgentError>>>>> = None;

    loop {
        // Promote the next queued intent when the single send slot is free. FIFO
        // ⇒ steer/follow-up submission order is preserved.
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
            // Drive the in-flight send only when one exists (precondition ⇒ the
            // `unwrap()` is never evaluated while `None`). On completion clear the
            // slot; a finished run makes the send resolve `Err(RunEnded)`, which we
            // discard — remaining intents then drain harmlessly as we exit.
            res = async { in_flight.as_mut().unwrap().await }, if in_flight.is_some() => {
                let _ = res;                 // Ok, or Err = run already ended: ignore
                in_flight = None;
            }
            maybe = input.next() => match keymap::translate_active(maybe) {
                InputAction::Abort        => control.abort(),                        // synchronous, never queued
                InputAction::Steer(t)     => intents.push_back(ControlIntent::Steer(t)),
                InputAction::FollowUp(t)  => intents.push_back(ControlIntent::FollowUp(t)),
                InputAction::Edit(e)      => { shell.apply_edit(e); shell.render(); }
                InputAction::Resize(c, r) => { shell.set_size(c, r); shell.render(); }
                InputAction::Closed       => { control.abort(); return Ok(RunEnded::Aborted); } // stdin gone
                InputAction::Ignore       => {}
            },
            ev = run.next() => match ev {
                Some(AgentEvent::TurnFinished { usage, .. }) => { shell.on_turn_finished(&usage); shell.render(); }
                Some(AgentEvent::RunFinished { reason, .. }) => return Ok(RunEnded::from(reason)),
                Some(other) => { shell.on_agent_event(&other); shell.render(); }
                None => return Ok(RunEnded::Aborted), // correction 7: unexpected end handled explicitly,
                                                      // NOT by disabling the arm. A started run always ends
                                                      // with RunFinished; a bare None is treated as aborted.
            },
            _ = ticker.tick() => { shell.animate(); shell.render(); }
        }
    }
}
```

Ownership: `intents` owns the queued `String`s; `in_flight` owns a `Pin<Box<dyn
Future + 'static>>` that captured a cloned `RunControl` + text — no borrow of
`control`/`shell`/loop locals, so it coexists with the `input`/`run`/`ticker`
borrows in the same `select!`. `TurnFinished.usage` now updates only the UI token
gauge (`shell.on_turn_finished`); it is **not** used for capacity math (Revision
2.1 correction 2). `wait_for_prompt` is the idle-phase analogue (a `select!` over
`input` + `ticker` only) returning `Idle::{Submit(String), Command(Command), Quit}`.

### 6.3 Tool execution with live progress

`ToolStarted` → create a `Panel` (name + parsed args), record its index.
`ToolProgress::Output` bytes are appended via `String::from_utf8_lossy` into a
bounded ring (UI keeps the last 64 KiB per panel). `ToolProgress::Dropped{bytes}`
appends "… N bytes elided". `ToolFinished{result}` finalizes the panel (`Ok`
collapses to a summary line; `Err` shows the error text — display only). **The
session stores the tool's bounded final `ToolOutput.text`** (already capped by the
sandbox `max_output_bytes`), *not* the unbounded live process output — live
progress is never persisted (`tool.rs`, `events.rs:57`). The UI is display-only.

### 6.4 Steering, follow-up, abort, and runtime reconfiguration

**Steer/follow-up/abort** are handled inside `drive_active_run` (§6.2). Frozen
semantics (`agent_run.rs::steer_arrives_during_continuous_progress`,
`follow_up_begins_after_the_run_settles`,
`abort_during_process_execution_kills_the_tool`): `steer` enters at the next
model-turn boundary; `follow_up` starts a turn after the run settles; `abort`
preserves committed entries and yields one `RunFinished{Aborted}`.

**Runtime reconfiguration (`/model`, `/thinking`, `/new`, `/resume`) is in v1.**
Each is applied only at an **idle boundary** through the single consuming
`rebuild_app` transition (§5.2), which drops the old `Agent` — closing the
session `File` — *before* opening/creating a session, so there are never two
owners of one session. A request issued **during a run is queued** and applied
after the run settles:

```rust
pub enum Reconfig {
    Model(ModelId),                 // /model <id> or picker result
    Thinking(ReasoningConfig),      // /thinking <level>, mapped capability-aware (below)
    NewSession,                     // /new
    Resume(PathBuf),                // /resume <id> or picker result
}

/// Applied only in the idle phase (no live Run). Returns the rebuilt App.
pub fn apply_reconfig(app: App, r: Reconfig) -> anyhow::Result<App> {
    match r {
        Reconfig::Model(id) => { let m = app.catalog.resolve(&id)?; rebuild_app(app, Some(m), None, None) }
        Reconfig::Thinking(rc) => rebuild_app(app, None, Some(rc), None),
        Reconfig::NewSession => {
            let path = app.sessions.new_path(&crate::modes::timestamp());
            rebuild_app(app, None, None, Some(SessionSelection::CreateNew(path)))
        }
        Reconfig::Resume(path) => rebuild_app(app, None, None, Some(SessionSelection::OpenExisting(path))),
    }
}
```

`/model` and `/thinking` keep the **same** session (history preserved, transcript
unchanged). `/new` and `/resume` switch sessions and re-hydrate the transcript
(§6.5). `/theme` is **not** here — it is TUI-only and never rebuilds the `Agent`
(§8.4).

**Capability-aware `/thinking` mapping.** A requested level maps to a
`ReasoningConfig` using the target model's advertised capability
(`model.spec.capabilities.reasoning`):

```rust
pub fn thinking_to_reasoning(level: ThinkingLevel, model: &Model) -> anyhow::Result<ReasoningConfig> {
    let cap = match &model.spec.capabilities.reasoning {
        None => return if matches!(level, ThinkingLevel::Off) { Ok(ReasoningConfig::Off) }
                       else { anyhow::bail!("{} has no thinking support", model.spec.id.0) },
        Some(c) => c,
    };
    Ok(match (level, cap.control) {
        (ThinkingLevel::Off, _)                       => ReasoningConfig::Off,
        (l, ReasoningControl::Effort)                 => ReasoningConfig::Effort(l.to_effort()),
        (l, ReasoningControl::TokenBudget)            => {
            let b = cap.effort_budgets.as_ref().ok_or_else(|| anyhow::anyhow!("no budgets"))?;
            ReasoningConfig::Budget(l.pick_budget(b))     // minimal/low/medium/high
        }
    })
}
```

The status bar shows the current level (`app.reasoning`); the `/thinking` picker
offers only levels the selected model supports.

### 6.5 Resume + transcript hydration (correction 5)

`--continue`/`--resume` choose the `Session` at bootstrap. After `build_app`,
`shell.hydrate(app.agent.session())` populates the transcript **before input is
accepted**, walking the head's ancestor chain (**active branch only**):

```rust
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall { id: ToolCallId, name: String, args: serde_json::Value },
    ToolResult { id: ToolCallId, text: String, is_error: bool },
    CompactionMarker { summary_preview: String },
}

/// Walk head → root via parent links (active branch), reverse to chronological,
/// and render each entry. Other branches present in `entries()` are excluded.
pub fn hydrate_transcript(session: &Session) -> anyhow::Result<Vec<TranscriptItem>> {
    let mut out_rev = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let entry = session.entry(&id).ok_or_else(|| anyhow::anyhow!("dangling entry {id:?}"))?;
        match &entry.value {
            EntryValue::Message(Message::User(u))       => push_user_items(&mut out_rev, u),      // Text and/or ToolResult parts
            EntryValue::Message(Message::Assistant(a))  => push_assistant_items(&mut out_rev, a), // Text / Reasoning / ToolCall parts
            EntryValue::Compaction { summary, .. }      => out_rev.push(TranscriptItem::CompactionMarker { summary_preview: preview(summary) }),
            EntryValue::Config { .. }                   => {}
        }
        cursor = entry.parent.clone();
    }
    out_rev.reverse();
    Ok(out_rev)
}
```

What comes from where on resume:

| Source | Provides |
|--------|----------|
| Persisted session (active branch via head chain) | rendered transcript, compaction markers, head |
| Current project config (`AGENTS.md`, `.ygg/`) | system prompt used *now* |
| Current resources | named prompts available *now* |
| Current model selection (§8 order) | model used *now* |

**Historical `EntryValue::Config{model,reasoning}` markers are record only.**
Current explicit CLI/project configuration always wins; a stale recorded value
never silently overrides an explicit flag. If the current model differs from the
last recorded one, the resume path writes a fresh `Config` marker for provenance.

### 6.6 Compaction — §9.

### 6.7 Print mode (persistent session — correction 6)

```rust
pub async fn run_print(boot: Bootstrap, prompt: String) -> anyhow::Result<()> {
    use std::io::Write;
    let launch = resolve_launch_print(&boot)?;                 // actionable error if unresolved; no picker
    let system = resources::compose_instructions(&boot.config)?;
    let mut app = build_app(boot, launch, system)?;            // creates/opens a NORMAL persistent session

    // Pre-request capacity gate applies here too (large resumed context + large prompt).
    if let CapacityDecision::Exceeded { estimate, budget } =
        ensure_capacity_before_prompt(&mut app, &prompt).await?
    {
        anyhow::bail!("prompt too large: ~{estimate} tokens exceeds the {budget}-token budget even after compaction");
    }

    // Copy the flag out before `run` borrows `app.agent`, so the match needs no field borrow.
    let show_reasoning = app.config.show_reasoning_in_print;

    let mut run = app.agent.prompt(prompt).await?;
    let mut out = std::io::stdout().lock();
    let mut finished: Option<RunEnded> = None;                 // explicit RunFinished tracking (Rev 2.1 c.5)
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::OutputDelta { channel: OutputChannel::Text, text }               => write!(out, "{text}")?,
            AgentEvent::OutputDelta { channel: OutputChannel::Reasoning, text } if show_reasoning => write!(out, "{text}")?,
            AgentEvent::RunFinished { reason, .. } => finished = Some(RunEnded::from(reason)),
            _ => {}
        }
    }
    drop(run);
    out.flush()?;

    match finished {
        Some(RunEnded::Completed)  => Ok(()),
        Some(RunEnded::MaxTurns)   => anyhow::bail!("run hit max turns before completing"),        // nonzero exit
        Some(RunEnded::Aborted)    => anyhow::bail!("run aborted before completing"),              // nonzero exit
        Some(RunEnded::Failed(e))  => anyhow::bail!("run failed: {e}"),                            // error
        None                       => anyhow::bail!("run stream ended without RunFinished (invariant violation)"),
    }
}
```

`main` maps any `Err` to a nonzero process exit, so `MaxTurns`/`Aborted`/`Failed`
and the invariant violation all surface as failures to a caller/CI.

Print mode creates a **normal persistent JSONL session** under the workspace's
session directory (same layout as interactive) and may be `--continue`d or
`--resume`d later. No temporary files, no "ephemeral" semantics. No TUI or
`Theme` is constructed.

### 6.8 Exit and crash recovery

Durable already (frozen `Session`): every completed message and each individual
tool result is written before its event is observed; a torn final line is
truncated on reopen. **May be lost on crash:** in-flight streaming deltas (never
persisted), the UI transcript (rebuildable by `hydrate_transcript`), and any
not-yet-written tool result (tools are at-least-once). Clean exit or panic:
`force_restore()` (idempotent, chained) restores the terminal; dropping `Run`
cancels in-flight model/tool work.

---

## 7. TUI event architecture

**Constraint.** `sexy_tui::TUI` is `!Send` (`Rc`/`RefCell`/`Box<dyn Component>`),
and `ProcessTerminal::start()` is a blocking loop that only wakes on terminal
input (`terminal.rs:151`). The agent `Run` is an async stream. Both cannot own
the thread.

**Resolution.** A single-threaded async loop on the main thread
(`#[tokio::main(flavor = "current_thread")]`). We do **not** call
`ProcessTerminal::start()`; we implement `YggTerminal` (render-only) and drive
input with `crossterm::event::EventStream` (the `event-stream` feature is enabled
in sexy-tui's manifest). The idle/active phase split (§6.2) keeps this compatible
with `Run<'a>`'s `&mut Agent` borrow.

- **Disjoint select borrows.** `input`, `run`, `ticker`, and the single in-flight
  control-send future are separate locals; each `select!` arm borrows one. Arm
  bodies borrow `shell`/`control` sequentially. This is the pattern that compiles.
- **Unexpected `Run::next() == None`** is handled explicitly (→ `RunEnded::Aborted`),
  never by disabling an arm.
- **Steering/abort never blocked:** input and `run.next()` are peer arms; sends are
  queued (§6.2).
- **Render cadence:** `render()` → `TUI::request_render()` → `do_render()`
  (verified: `tui.rs`), which diff-renders and self-throttles
  (`MIN_RENDER_INTERVAL_MS = 16`). Any code path (agent event, ticker, key) can
  call it — proven sufficient for **non-input async wakeups** by Gate 0.
- **Resize (verified mechanism).** `TUI::do_render` reads `terminal.columns()/rows()`
  and does **not** re-query the OS; the `YggTerminal` is boxed inside the `TUI` and
  unreachable. So `YggTerminal` holds its dimensions in an `Rc<Cell<(u16,u16)>>`
  and the shell keeps a clone (single-threaded, `!Send`-safe). On `Event::Resize`
  the shell writes the cell, then `request_render()`; `columns()/rows()` read it.
- **Pickers (verified).** `SelectList` implements `Component::handle_input(&str)`
  and matches `Key::up/down/enter/escape` against the exact strings the (private)
  `key_to_string` emits. The shell **owns** the active `SelectList` (not moved into
  the `TUI`), feeds it keys via a **replicated key encoder** (§5.5), composites its
  `render(width)` output, and reads `selected_item()` on Enter. No overlay-focus
  machinery is required.
- **Restore:** `force_restore()` is idempotent (`RAW_ACTIVE` atomic) and chained
  into the prior panic hook (§5.4).
- **Markdown/reasoning:** assistant text blocks are `sexy_tui::Markdown` (syntect,
  built via a `MarkdownTheme` helper — §5.5); reasoning is a separate dim block.
- **Bounded UI memory:** last 64 KiB per tool panel; the authoritative **bounded**
  final result is in the session (§6.3).

Nothing terminal/rendering-related appears in `ygg-agent`.

### 7.1 sexy-tui integration helpers (§5.5 detail)

`MarkdownTheme` and `SelectListTheme` have **no `Default`** (each is a struct of
`Box<dyn Fn(&str) -> String>` closures; only `MarkdownOptions` derives `Default`).
They are built the way `EditorTheme::new(&Theme)` is (verified pattern,
`widgets/editor.rs`): clone the sexy-tui `Theme` (it is `Clone`) into `move`
closures calling `theme.fg(token, s)` / `theme.bold(s)`. The coding-agent's
`tui/theme.rs` provides `markdown_theme(&Theme) -> MarkdownTheme`,
`select_list_theme(&Theme) -> SelectListTheme`, and reuses
`EditorTheme::new(&Theme)`. The key encoder (`keymap::encode(&KeyEvent) -> String`)
replicates the private `key_to_string` (arrows `\x1b[A/B/C/D`, Enter `\r`, Esc
`\x1b`, printable chars, kitty `\x1b[<cp>;<mod>u`) so `SelectList::handle_input`
and `matches_key` recognize the input.

---

## 8. Resource and configuration rules

### 8.0 Workspace resolution (correction 4)

```
workspace = --workspace                         (if provided)
          | nearest ancestor of cwd with .git   (walk up)
          | cwd                                  (fallback)
then canonicalize(workspace)
```

`invocation_cwd = canonicalize(cwd)` and must be inside `workspace` (if a
`--workspace` override excludes cwd, the deeper `AGENTS.md` walk simply starts at
`workspace`). **Project-instruction discovery never ascends above `workspace`.**

### 8.1 Configuration precedence

```
compiled defaults
  → global config   ~/.ygg/config.toml
  → project config  <workspace>/.ygg/config.toml
  → environment     (YGG_* knobs; API keys via ygg-ai Auth)
  → CLI arguments   (highest)
```

Resolved once into `Config`. API credentials are not a config layer — resolved by
`ygg-ai` `Auth` from env vars named by the catalog. TOML keys: `model`,
`reasoning`, `theme`, `allow_shell`, `exec_timeout_secs`, `max_output_bytes`,
`session_dir`, `compaction.threshold_fraction`, `compaction.keep_recent_turns`,
`max_turns`.

### 8.2 Model resolution order (correction 3, prior round)

```
explicit CLI --model
  → project config model
  → global config model
  → interactive bootstrap picker     (interactive only; SelectList over ModelCatalog::models())
```

Print mode with no model configured is an **actionable error**
(`resolve_launch_print`), never a picker.

### 8.3 `AGENTS.md` composition — deterministic, root-to-leaf (correction 4)

`AGENTS.md` is **automatic system context**. Discovery is workspace-scoped and
composition is a single ordered concatenation — **no "closest wins" language**:

```
compose in order (each appended after the previous; deeper = later = more specific):
  1. global      ~/.ygg/AGENTS.md
  2. workspace root   <workspace>/AGENTS.md
  3. each nested <dir>/AGENTS.md for dir on the path workspace → invocation_cwd
       (workspace-relative, ascending toward cwd; NEVER above workspace)
```

```rust
pub fn compose_instructions(config: &Config) -> anyhow::Result<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(g) = read_if_exists(global_agents_md()?) { parts.push(g); }       // ~/.ygg/AGENTS.md
    for dir in dirs_from_workspace_to_cwd(&config.workspace, &config.invocation_cwd) {
        if let Some(s) = read_if_exists(dir.join("AGENTS.md")) { parts.push(s); }  // root … leaf
    }
    Ok(compose_with_base_persona(parts))    // base coding persona first, then parts joined by blank lines
}
```

Later (deeper) sections are more specific and, by convention, take precedence
over earlier ones — but **all discovered instructions are included**; nothing is
dropped. `dirs_from_workspace_to_cwd` yields `workspace`, then each successive
child down to `invocation_cwd` (inclusive), so the workspace root is always first
and never exceeded.

### 8.4 Resource semantics and command grammar (Revision 3)

**Two resource kinds only** — a named-prompt/skill subsystem is **deferred**
(`AGENTS.md` is sufficient project-instruction support for v1):

| Resource | Discovery | Format | Loaded when | Enters | Live reload |
|----------|-----------|--------|-------------|--------|-------------|
| **`AGENTS.md`** | §8.3 (workspace-scoped) | Markdown | startup, composed | **system prompt** | no |
| **Themes** | `~/.ygg/themes/*.toml`, `<workspace>/.ygg/themes/*.toml` | TOML | startup + `/theme` | **TUI only** | yes |

**Command grammar (v1, in-TUI slash commands).** Editor text beginning with `/`
is parsed as a command; otherwise it is submitted as a prompt. Commands with a UI
selector open a **picker** (an owned `SelectList`, §7); reconfiguration runs at an
idle boundary (queued if requested mid-run, §6.4). These are TUI commands, **not**
shell CLI commands.

```
/model [id]                       no arg → model picker; with id → resolve + switch (consuming transition)
/thinking [off|min|low|med|high]  no arg → capability-aware level picker; with level → switch (consuming)
/theme [name]                     no arg → theme picker; with name → apply now (TUI-only, no Agent rebuild)
/compact                          force compaction now at idle; report outcome (§9.4)
/new                              create a new persistent session; switch UI (consuming transition)
/resume [id]                      no arg → session picker; with id → resume (consuming transition + hydrate)
/status                           model, thinking, workspace, session id/title, context est. + budget,
                                  security block (trusted local agent, workspace path guard, capability
                                  gates, OS isolation: none), whether a follow-up/reconfig is queued
/help                            slash commands + submit/steer/follow-up/abort/scroll bindings (overlay)
/quit                            abort active work if any → restore terminal → exit
<anything else>                  submit as a prompt
```

- `/compact` runs in the idle phase (full `&mut App`), uses only frozen `Session`
  APIs, and requires no `Agent` rebuild.
- `/model`, `/thinking`, `/new`, `/resume` run through the consuming `rebuild_app`
  transition (§6.4); if invoked during a run they are queued and applied when it
  settles.
- `/theme` never rebuilds the `Agent`; a theme failure is a TUI-only notice and
  cannot affect print mode.
- **Removed from v1:** `/checkout` (raw entry IDs are poor daily UX;
  `Session::checkout` stays an internal primitive for a future branch/fork UI) and
  `/prompt` (named prompts deferred). See the backlog.

---

## 9. Compaction design (corrections 1, 3)

Policy lives in `ygg-coding-agent`; the mechanism (`Session::compact`) is frozen.

### 9.1 Verified `Usage` semantics (correction 3)

**Verified against the code and existing tests — `input_tokens` and
`cache_read_tokens` are ADDITIVE, not overlapping:**

- Anthropic: `input_tokens` already excludes cache (`anthropic.rs:883`); canonical
  `total = input + cache_read + cache_write + output` (`anthropic.rs:901–905`).
- OpenAI Chat: `input = prompt_tokens − cached_tokens` (`openai_chat.rs:962`).
- OpenAI Responses: `input = input_tokens − cache` (`openai_responses.rs:1057`).
- `reasoning_tokens` is a subset of `output_tokens`; `cache_write_1h` is a subset
  of `cache_write`.

This is **already pinned by the frozen test**
`final_usage_merges_cache_and_thinking_tokens` (`anthropic.rs:1242–1256`), which
asserts `input=100, cache_read=40, cache_write=10, output=50, total=200`. No new
`ygg-ai` test is needed.

**Where this is used:** the exact, protocol-agnostic occupancy of a *completed*
request is `total_tokens − output_tokens` (= `input + cache_read + cache_write`,
correctly including cache-**write**). Per Revision 2.1 correction 2 this drives
**only the UI token gauge** (`shell.on_turn_finished`) — the pre-request capacity
gate does **not** use provider `Usage`, because `TurnFinished.usage` is
run-cumulative and would misstate a single request's size in a multi-turn run.
The gate instead estimates the next request directly (§9.2).

### 9.2 Pre-request capacity gate (correction 3)

A post-run check cannot protect a resumed session already near capacity, nor a
very large next prompt. Compaction is a **pre-request gate**:

```
user submits prompt
→ estimate the NEXT complete model request
→ compact if required
→ Agent::prompt
```

The estimator **reconstructs and estimates the complete next request** — no
`Usage` calibration (Revision 2.1 correction 2). Run-cumulative
`TurnFinished.usage` is *not* a single request's occupancy in a multi-turn run,
and clearing it between runs would leave nothing to calibrate the pre-request
gate; so provider `Usage` feeds only the **UI token gauge**, never the estimate.

```rust
/// Estimate the tokens of the NEXT complete request from what will actually be sent.
pub fn estimate_next_request_tokens(app: &App, pending_prompt: &str) -> u64 {
    // Active-branch context the Agent will reconstruct for this request.
    let context = app.agent.session().context().map(|ms| estimate_messages_tokens(&ms)).unwrap_or(0);
    context
        .saturating_add(app.system_tokens)             // system instructions (precomputed)
        .saturating_add(app.tool_schema_tokens)        // tool-schema reserve (precomputed, §5.2)
        .saturating_add(estimate_text_tokens(pending_prompt))
        .saturating_add(FRAMING_OVERHEAD_TOKENS)       // request framing reserve
}

// Conservative char→token estimate (~4 bytes/token) + per-message framing.
pub fn estimate_text_tokens(s: &str) -> u64 { (s.len() as u64 + 3) / 4 }
pub fn estimate_messages_tokens(ms: &[Message]) -> u64 {
    ms.iter().map(|m| message_byte_len(m) as u64 / 4 + PER_MESSAGE_OVERHEAD_TOKENS).sum()
}
```

**Budgets.** Reserve room for the model's own output. The *hard* input budget is
the context window minus the reserved output; the *soft* threshold triggers
proactive compaction below it:

```rust
pub fn hard_input_budget(model: &Model) -> u64 {
    model.spec.limits.context_window
        .saturating_sub(model.spec.limits.max_output_tokens)   // types.rs:161
}
pub fn soft_threshold(model: &Model, policy: &CompactionPolicy) -> u64 {
    (policy.threshold_fraction * hard_input_budget(model) as f64) as u64   // default 0.85
}
```

The pre-request gate is the only capacity check; a post-run proactive check may
be added later as an optimization but never replaces it.

### 9.3 Semantic turn boundary — never orphan tool results (correction 3)

`first_kept` must **not** be chosen by raw entry count. It must be a semantic
boundary that (a) never orphans a tool result and (b) never separates an
assistant tool call from its results. **Invariant: `first_kept` is an *assistant*
entry** on the active branch. This is exactly the shape the frozen
`Session::context()` expects — the injected summary is a synthetic *user* message,
so the kept range must start with an *assistant* message to preserve user→assistant
alternation (proven by `session.rs::compaction_reconstruction_matches_design_example`,
where `first_kept = E2` is an assistant). Starting at an assistant also guarantees
its tool results (which always follow) are inside the kept range, so nothing is
orphaned.

```rust
/// Choose the assistant entry that begins the K-th-most-recent user turn, on the
/// active branch. Returns None when there is nothing safe/worthwhile to compact.
pub fn choose_first_kept(session: &Session, keep_recent_turns: usize) -> Option<EntryId> {
    // 1. Active branch, chronological.
    let chain = active_branch_chrono(session);            // Vec<&Entry> root→head via head's parent chain

    // 2. Turn starts = assistant entries whose parent is a user-TEXT message
    //    (a genuine prompt/steer/follow-up), i.e. the first assistant reply of a turn.
    let turn_start_assistants: Vec<EntryId> = chain.iter()
        .filter(|e| is_assistant(e) && parent_is_user_text(session, e))
        .map(|e| e.id.clone())
        .collect();

    // 3. Keep the last `keep_recent_turns` turns; boundary = start of the oldest kept turn.
    if turn_start_assistants.len() <= keep_recent_turns { return None; } // nothing worthwhile
    let idx = turn_start_assistants.len() - keep_recent_turns;
    let first_kept = turn_start_assistants[idx].clone();

    // 4. Guards: must have a non-empty summarized prefix and not equal the head.
    if Some(&first_kept) == session.head().as_ref() { return None; }
    Some(first_kept)                                       // ancestor-of-head by construction
}
```

`active_branch_chrono` walks `head → root` via `entry.parent` and reverses.
`parent_is_user_text` checks the parent entry is `Message::User` whose parts are
all `UserPart::Text` (distinguishing a real prompt from a tool-result user
message). If no clean boundary exists (e.g. a single enormous turn), the function
returns `None` and compaction is skipped rather than producing an invalid context.

**Boundary tests (coding-agent unit tests):** construct sessions and assert —
(1) `first_kept` is always an assistant entry; (2) the summarized prefix never
ends mid-tool-call (the entry before `first_kept` is a completed user turn);
(3) after `session.compact(summary, first_kept)`, `session.context()` has valid
user/assistant alternation and contains no tool-result message lacking a
preceding assistant tool call; (4) a session with a single huge turn yields
`None` (skip); (5) checkout to an earlier branch then compact selects the boundary
on the *active* branch only.

### 9.4 The gate: soft compaction + hard-capacity failure (correction 3, Rev 2.1)

Compaction failures (`None` boundary, summarizer error, `compact` error) are
non-fatal **only if the request still fits the hard budget**. A single enormous
turn that cannot be compacted and does not fit produces an actionable
`CapacityDecision::Exceeded`, and the caller does **not** call `Agent::prompt`.

```rust
pub enum CompactionOutcome { NotNeeded, Compacted { elided: usize }, Skipped { reason: String } }

pub enum CapacityDecision {
    Proceed(CompactionOutcome),               // fits the hard budget (possibly after compaction)
    Exceeded { estimate: u64, budget: u64 },  // still over hard budget → do NOT prompt
}

/// Between-runs, before Agent::prompt. Full &mut App (no live Run).
pub async fn ensure_capacity_before_prompt(
    app: &mut App, pending_prompt: &str,
) -> anyhow::Result<CapacityDecision> {
    let hard = hard_input_budget(&app.model);
    let soft = soft_threshold(&app.model, &app.config.compaction);

    let estimate = estimate_next_request_tokens(app, pending_prompt);
    if estimate <= soft {
        return Ok(CapacityDecision::Proceed(CompactionOutcome::NotNeeded));
    }

    // Soft threshold exceeded → attempt compaction (non-fatal on skip).
    let outcome = attempt_compaction(app).await?;

    // Recompute against the HARD budget and decide.
    let recomputed = estimate_next_request_tokens(app, pending_prompt);
    if recomputed <= hard {
        Ok(CapacityDecision::Proceed(outcome))
    } else {
        Ok(CapacityDecision::Exceeded { estimate: recomputed, budget: hard })
    }
}

/// One compaction attempt. Always non-fatal: returns Compacted or Skipped.
async fn attempt_compaction(app: &mut App) -> anyhow::Result<CompactionOutcome> {
    let first_kept = match choose_first_kept(app.agent.session(), app.config.compaction.keep_recent_turns) {
        Some(id) => id,
        None => return Ok(CompactionOutcome::Skipped { reason: "no safe turn boundary to compact".into() }),
    };
    let to_summarize = messages_before(app.agent.session(), &first_kept)?;   // ancestors of first_kept, chrono
    let summary = match summarize(&app.client, &app.model, &to_summarize).await {
        Ok(s)  => s,
        Err(e) => return Ok(CompactionOutcome::Skipped { reason: e.to_string() }),   // non-fatal
    };
    match app.agent.session_mut().compact(summary, first_kept) {             // validates ancestor-of-head
        Ok(_)  => Ok(CompactionOutcome::Compacted { elided: to_summarize.len() }),
        Err(e) => Ok(CompactionOutcome::Skipped { reason: e.to_string() }),
    }
}
```

`summarize` builds a `ygg_ai::Request` directly (system = a summarization
instruction, `messages = to_summarize`, `tools = []`, `tool_choice =
ToolChoice::None`, `reasoning = Off`, `output_format = Text`) and calls
`app.client.complete(&app.model, req)` — a **separate** `AiClient::complete` that
never touches the `Agent`'s in-flight state (`AiClient` is `Clone`,
`client.rs:19`). `messages_before` walks `first_kept`'s parent chain to root,
collecting messages (skipping `Config`, folding any nested `Compaction` summary),
reversed to chronological.

**Caller behavior.** Interactive (`run_interactive`, §6.2): `Exceeded` shows an
actionable error and `continue`s without prompting; `Proceed` reports the outcome
and prompts. Print (`run_print`, §6.7): `Exceeded` returns an `Err`. `Compacting…`
is shown during the await.

---

## 10. Session UX and storage layout

### 10.1 Directory layout (filesystem, no DB)

```
~/.ygg/
  config.toml
  AGENTS.md                      # global instructions (composed first)
  themes/<name>.toml
  prompts/<name>.md
  sessions/
    <workspace-key>/             # short stable hash of the canonical workspace path
      2026-07-12T14-30-05Z-a1b2.jsonl
      2026-07-12T16-02-11Z-c3d4.jsonl
```

`<workspace-key>` scopes sessions per project. The JSONL file is the frozen
`Session` format, unchanged. Interactive and **print** sessions live here alike.

### 10.2 Commands and selection behavior

| Entry point | Behavior |
|-------------|----------|
| `ygg` | new session in this workspace |
| `ygg "fix the failing tests"` | new session, seeded with that first prompt |
| `ygg --print "explain this repo"` | print mode, **new persistent session** (resumable) |
| `ygg --continue` | open newest session (by mtime) for this workspace |
| `ygg --resume` | session picker overlay (interactive); id required in print |
| `ygg --resume <id>` | open that session by id (filename stem) |

```rust
impl SessionStore {
    pub fn new(session_dir: &Path, workspace: &Path) -> Self;   // computes workspace-key dir
    pub fn new_path(&self) -> PathBuf;                          // timestamped filename
    pub fn latest(&self) -> anyhow::Result<SessionMeta>;       // newest by mtime; err if none
    pub fn by_id(&self, id: &str) -> anyhow::Result<SessionMeta>;
    pub fn list(&self) -> Vec<SessionMeta>;                    // newest-first, for the picker
}
pub struct SessionMeta { pub id: String, pub path: PathBuf, pub modified: SystemTime, pub title: String }
```

### 10.3 Titles — derived from the active branch (correction 5)

The display title is generated from the session's **active branch**, not from the
first physical JSONL lines (after a `checkout`, the active branch's first prompt
may not be the earliest entry in the file). The title is the **oldest
user-text message on the head's ancestor chain**, trimmed to ~60 chars:

```rust
pub fn active_branch_title(session: &Session) -> String {
    // Walk head → root; remember the last-seen user-text (deepest ancestor = oldest on the branch).
    let mut oldest_user_text: Option<String> = None;
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let e = match session.entry(&id) { Some(e) => e, None => break };
        if let EntryValue::Message(Message::User(u)) = &e.value {
            if let Some(t) = first_text_part(u) { oldest_user_text = Some(t); }  // keep updating; deepest wins
        }
        cursor = e.parent.clone();
    }
    oldest_user_text.map(|t| trim_title(&t)).unwrap_or_else(|| "(empty session)".into())
}
```

This requires opening/replaying the session (`Session::open`), **not** reading a
fixed few lines. For the picker list this is `N` session reads for `N` sessions in
the workspace — acceptable at v1 scale (local, small `N`); no sidecar and no title
mutation. Manual rename is deferred (the only feature that would justify a mutable
sidecar).

### 10.4 Branch / checkout (internal primitive only)

Frozen `Session::checkout(EntryId)` forks a new branch while preserving the old
(`agent_run.rs::end_to_end_session_invariant`). It remains an **internal
foundation primitive**; v1 does **not** expose `/checkout` or raw entry IDs — a
branch/fork UI over visible transcript messages (and a session tree) is post-v1
(see the backlog). `/resume` re-hydrates to the selected session's active branch.

---

## 11. Risk review

| Risk | How it would happen | Preventative rule |
|------|---------------------|-------------------|
| **God object** | An `App`/`CodingSession` accretes model+session+tui+theme+config+compaction | `App` holds mode-agnostic *state* only; TUI/Theme live in `InteractiveShell`; behavior is free functions. §1/§3/§5.2. |
| **Borrow-incompatible loop** | Holding `&mut App` while a `Run` borrows `&mut agent` | Explicit idle/active phases; `drive_active_run` never receives `&mut App`. §6.2. |
| **Second agent runtime** | Re-implementing the turn/tool loop or request building | Only `Agent::prompt`/`complete` run turns. The sole direct `AiClient` use is the stateless compaction summarizer (§9.4), which runs no tools. |
| **Event-bus framework** | Generic pub/sub over `AgentEvent` | One consumer (`InteractiveShell::on_agent_event`) in one `select!`. No dispatcher/registry. |
| **Resource labyrinth** | Override callbacks, per-category precedence, hot-reload everywhere | Three flat resource kinds; one precedence rule (project>global); reload only for themes. §8.4. |
| **Partial clone of Pi** | Porting `AgentSession`'s responsibility set | Reference Pi for behavior, not structure; deferred list removes RPC/dialogs/tree. |
| **Underpowered demo** | Ships without steering/resume/compaction/tools/model+thinking controls | v1 = the complete daily-usable interactive product (§2, all 19 rows), not a fresh-session slice; §12 sequences it end to end. |
| **Two owners of one session** | Reopening a session while the old `Agent` still owns its `File` (runtime `/model`,`/thinking`,`/new`,`/resume`) | `rebuild_app` is a *consuming* transition: it `drop`s the old `Agent` before opening/creating the session (§5.2/§6.4); reconfig requested mid-run is queued to an idle boundary. |
| **TUI unfoundable** | `!Send` TUI + blocking `ProcessTerminal::start` | **Gate 0** spike proves the model before finalizing; failure is architecture-changing. §12. |
| **Silent config override on resume** | Recorded `Config` marker overrides current CLI | Historical markers are record-only; current explicit config wins. §6.5. |
| **Unsafe live Agent reconstruction** | Reopening a session while the old `Agent` owns the File | Deferred; if added, a consuming transition drops the old owner first. §6.4. |
| **Compaction overflow / orphaned tool results** | Wrong metric or count-based boundary | Occupancy = `total − output` vs `context_window − max_output_tokens`; `first_kept` is a semantic assistant boundary; both tested. §9. |
| **Terminal left corrupted** | Panic/abort mid-run | Idempotent `force_restore` via `RAW_ACTIVE`, chained into the prior panic hook. §5.4. |
| **Unbounded UI memory** | Long tool output retained forever | Last 64 KiB/panel; authoritative bounded result is in the session. §6.3/§7. |
| **Lost transcript on resume** | Blank UI on `--resume` | `hydrate_transcript` on the active branch before accepting input. §6.5. |

---

## 12. Implementation sequence

**Compile-oriented self-review of the Rust sketches (done before this section):**
`App` never holds `Theme`/TUI; `drive_active_run` takes no `&mut App` and its
`select!` arms borrow disjoint locals (`input`/`run`/`ticker`); `Run::next() == None`
is matched explicitly; `From<FinishReason> for RunEnded` covers all four variants;
`estimate_next_request_tokens` uses only public `Session::context`/`Model.spec.limits`;
`choose_first_kept` uses only public `Session` accessors and returns `Option`;
`force_restore` is idempotent (atomic swap) and the panic hook is chained;
`summarize` uses public `AiClient::complete`; every `?` sits in an
`anyhow::Result`/`AgentError`-returning fn. Remaining `...`/helper names
(`push_user_items`, `active_branch_chrono`, `message_byte_len`, etc.) are trivial
private helpers, not new abstractions.

### Gate 0 — TUI integration spike (correction 4; run first)

**Goal:** prove sexy-tui renders and takes input **without**
`ProcessTerminal::start()`, via `YggTerminal` + `crossterm::EventStream` +
current-thread runtime, with resize and correct restore — **and explicitly prove
`TUI::start()` does not call `Terminal::start()`.**

**Deliverable:** `src/spike/bin_spike.rs` (standalone `[[bin]]`). Sketch:

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    install_panic_hook();
    let term = YggTerminal::enter()?;                 // raw + alt screen + hide cursor; RAW_ACTIVE=true
    let mut tui = sexy_tui::TUI::new(Box::new(term));
    tui.add_child(Box::new(sexy_tui::Markdown::new(
        "# spike\ntype; `q` quits; resize the window.", Default::default(), Default::default())));
    tui.start();                                       // MUST NOT reach YggTerminal::start (which is unreachable!())
    let mut input = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut buf = String::from("# spike\n");
    loop {
        tokio::select! {
            maybe = input.next() => match maybe {
                // Ctrl+C in RAW mode arrives as a KEY event, NOT SIGINT — handle it
                // explicitly and exit through normal cleanup (Rev 2.1 correction 6).
                Some(Ok(Event::Key(k)))
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) => break,
                Some(Ok(Event::Key(k))) if k.code == KeyCode::Char('q') => break,
                Some(Ok(Event::Key(k))) => { if let KeyCode::Char(c) = k.code { buf.push(c); set_markdown(&mut tui, &buf); } }
                Some(Ok(Event::Resize(c,r))) => { set_size(&mut tui, c, r); tui.request_render(); }
                Some(Err(_)) | None => break,
                _ => {}
            },
            _ = ticker.tick() => tui.request_render(),  // proves paint on a NON-input async wakeup
        }
    }
    tui.stop();          // normal cleanup path; YggTerminal::drop → force_restore() also runs
    Ok(())
}
```

**Pass criteria (all):**
1. Renders/updates on keypress with no call to `ProcessTerminal::start()`.
2. **`tui.start()` does not invoke `Terminal::start()`** — verified structurally
   because `YggTerminal::start` is `unreachable!()`; reaching it panics the spike.
   (Confirmed by reading `tui.rs:444`: `TUI::start` only calls `hide_cursor` +
   `request_render`.) The spike must run past `tui.start()` without panicking.
3. The `ticker` arm proves paint on a non-input async wakeup (the mechanism agent
   events use).
4. `Event::Resize` reflows without corruption.
5. **Ctrl+C is handled as an explicit raw-mode key event** (`Char('c')` +
   `CONTROL`), not relied upon as an OS signal — raw mode does not deliver SIGINT —
   and exits through the same `tui.stop()` cleanup. After `q`, Ctrl+C, or a panic,
   the terminal is fully restored (idempotent `force_restore`) and the shell
   prompt is normal afterward.
6. Runs on `flavor = "current_thread"` (no `Send` requirement surfaced).

**If any criterion fails**, treat it as architecture-changing: fall back to a
dedicated blocking-input thread bridged by a channel, or a non-blocking driver
contributed to sexy-tui. Do not proceed until Gate 0 is green.

### v1 milestone sequence (authoritative plan: `plans/2026-07-12-ygg-coding-agent-v1.md`)

v1 is **not** defined by a fresh-session "Slice 1"; it runs from Gate 0 through the
complete daily-usable interactive product. Internal TDD milestones (each with its
own tests-first tasks in the v1 plan):

1. **Gate 0** — real-TTY `sexy-tui-rs` integration (above).
2. Crate, launcher args, workspace resolution, layered config, session paths.
3. Core interactive shell + `AgentEvent` mapping (streaming text + separate
   thinking, live tool panels, status bar, `Rc<Cell>` resize, restore).
4. Steering, ordered queued follow-ups, abort, control-send backpressure.
5. Persistent print mode (secondary) + explicit `RunFinished` classification.
6. Session discovery, active-branch hydration, `--continue`/`--resume`, `/new`.
7. Startup + runtime `/model` (picker + consuming transition + provenance).
8. Startup + runtime `/thinking` (capability-aware + consuming transition).
9. Automatic pre-request compaction + `/compact` (soft + hard-capacity).
10. `AGENTS.md` composition + layered configuration.
11. Theme loading, `markdown_theme`/`select_list_theme` helpers, picker, `/theme`.
12. `/status`, `/help` overlay, `/quit`, visible persistent errors.
13. Full regression tests (config, session store, compaction boundaries, keymap
    encoder, reconfig transitions) + scripted `AgentEvent` e2e.
14. Real-TTY / live-provider acceptance gate.

Testing spans every milestone: unit tests for the pure logic (config precedence,
workspace resolution, session store + `active_branch_title`, compaction occupancy +
`choose_first_kept` boundary invariants §9.3, keymap encoder, `thinking_to_reasoning`);
a scripted end-to-end test reuses the frozen wiremock pattern to drive interactive
event handling with a fake `AgentEvent` source; and manual real-TTY gates for the
spike (M1) and acceptance (M14).

---

## 13. Final recommendation

> **Will this design produce a genuinely useful coding agent that feels like
> Terminus's simplicity combined with Pi's usability and modularity?**

**Yes.**

- **Terminus simplicity:** a mode-agnostic `App` of state, an explicit idle/active
  procedural loop, and free functions — no framework, no god object, no second
  runtime. The frozen `Agent` is the core; the product is a thin, readable shell
  whose control flow follows top-to-bottom.
- **Pi usability:** streaming text *and* reasoning, live collapsible tool output,
  steering, queued follow-ups, clean abort, resume with transcript hydration,
  branch/checkout, pre-request compaction, workspace-scoped `AGENTS.md`, model
  picking, and TOML themes with live reload — all over one runtime shared with a
  persistent print mode.
- **Pi modularity, restrained:** three flat resource kinds, compile-time
  extensions through the frozen `ExtensionHost`, swappable TOML themes. No
  override labyrinth, no plugin ABI, no service container.
- **Ygg restraint:** local-first, filesystem-only, no database or cloud, small
  enough for one engineer to hold in their head. The one genuinely hard
  integration (`!Send` TUI + async agent) is de-risked first by Gate 0, with an
  explicit fallback.

Buildable from Gate 0 and layered to a complete daily-usable v1 without ever
growing a central object or a parallel agent loop.

---

## Appendix A — frozen API touchpoints (citations)

- `Agent::new/prompt/complete/session/session_mut` — `ygg-agent/src/agent.rs`
- `AgentConfig{.., reasoning}` — `agent.rs` (post reasoning-fix)
- `Run::next/control`, `RunControl::steer/follow_up/abort` — `agent.rs:112–172`
- `AgentEvent`, `OutputChannel`, `FinishReason` — `ygg-agent/src/events.rs`
- `Session::create/open/append/checkout/compact/context/head/entry/entries/path`,
  `EntryValue::{Message,Compaction,Config}`, `Entry{id,parent,value}` — `session.rs`
- `Session::context` alternation + coalescing; `compaction_reconstruction_matches_design_example` — `session.rs`
- `ExtensionHost`, `CoreTools`, `Tool`, `ToolDef`, `SandboxConfig` — `ygg-agent`
- `AiClient::new/complete/stream` (Clone) — `ygg-ai/src/client.rs:19`
- `ModelCatalog::builtin/resolve/models`, `Model{spec,endpoint}`,
  `ModelSpec.limits.{context_window,max_output_tokens}` — `catalog.rs`, `types.rs:161`
- `Usage` additive semantics + `total = input+cache_read+cache_write+output` —
  `types.rs:629`; proven by `anthropic.rs:1242 final_usage_merges_cache_and_thinking_tokens`;
  normalization at `anthropic.rs:883`, `openai_chat.rs:962`, `openai_responses.rs:1057`
- `ReasoningConfig`, `ToolChoice::None`, `Request`, `Message`, `UserPart` — `types.rs`
- `Auth` env credential resolution — `auth.rs`, `models/catalog.json`

## Appendix B — sexy-tui-rs API touchpoints (rust-port branch)

- `TUI::new/start/stop/add_child/children_mut/show_overlay/hide_overlay/handle_input/request_render/set_focus`
- `TUI::start` only calls `hide_cursor` + `request_render` (does **not** call `Terminal::start`) — `tui.rs:444`
- `Terminal` trait (implemented by `YggTerminal`) — `terminal.rs`
- `ProcessTerminal::start` is blocking / input-only (**not used**) — `terminal.rs:151`
- `Component{render,handle_input,invalidate}`, `Container`, overlays — `tui.rs`
- `Theme::load/fg/bg/model_color/spinner_frames/override_token` (TUI-only) — `theme/`
- Widgets: `Markdown`, `Editor`(+`EditorTheme::new(&Theme)`), `SelectList`,
  `Loader`, `Panel`, `Text`(shimmer), `Spacer` — `widgets/`
- crossterm `event-stream` feature enabled in sexy-tui's `Cargo.toml`
