# `ygg-agent` — Technical Design

> Stateful agent loop with tool execution and event streaming for Ygg.
> Status: **implemented (v0.1)** in `crates/ygg-agent`; corrections found
> during implementation are marked "(Correction: …)" in place.
> Companion to `crates/ygg-ai` (provider-independent inference).

---

## 0. Relationship to `ygg-ai`

`ygg-agent` sits above `ygg-ai`. The AI crate turns `Request` → `ResponseStream` →
`StreamEvent`. The agent crate orchestrates that stream: it builds requests from
conversation history, executes tool calls, feeds results back, persists sessions,
and emits agent-level events to consumers.

The agent crate uses `ygg-ai`'s public types (`Message`, `Request`, `StreamEvent`,
`ToolCall`, `ToolResult`, `Usage`, etc.) and never touches `ygg-ai::protocol`.

---

## 1. Design principles

1. **Every required product capability, smallest durable primitive.** Sessions,
   extensions, follow-ups, and secure tool boundaries are structural, not polish.
   But each is exactly as large as its job — no managers, backends, or trait
   hierarchies.
2. **Concrete, not abstract.** One session format (JSONL). One sandbox config
   (struct). Extension registration is one method with two hooks. Extract traits
   only when a second implementation exists.
3. **Procedural, not state-machine.** The agent loop is an async function with
   local variables. State lives in control flow, not in an enum.
4. **Persistence at completed-message boundaries.** Append user messages,
   assistant messages, and tool results to the session when they are complete.
   Never persist streaming deltas.
5. **`&mut self`, one authoritative session head.** The agent owns the session.
   No `Arc<Mutex<Session>>`, no detached tasks, no cloned state.
6. **Sequential tool execution by default.** Execute tool calls in emitted order.
   The only safe parallelization is contiguous `read`/`search` calls, and that
   can wait.
7. **Compact, line-oriented tool output.** Tools return plain text optimized for
   LLM consumption, not verbose JSON.
8. **At-least-once tool execution after crash.** Persist each tool result
   immediately. After an unclean exit, a tool that had a side effect but wasn't
   persisted may re-execute on resume.

---

## 2. Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│ Caller (TUI, print mode, RPC)                                         │
│   let mut agent = Agent::new(config)?;                                │
│   let mut run = agent.prompt("Fix the bug")?;                        │
│   let control = run.control();                                        │
│                                                                       │
│   loop { tokio::select! {                                            │
│       event = run.next() => render(event),                            │
│       input = user_input => control.steer(...),                       │
│   }}                                                                  │
└───────────────────────────────┬──────────────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────────────┐
│ Agent                                                                 │
│   client: AiClient                                                    │
│   model: Model                                                        │
│   session: Session          ←─ concrete JSONL, parent-linked entries  │
│   extensions: ExtensionHost ←─ tools + event observers               │
│   sandbox: SandboxConfig    ←─ workspace, allow_edit/process/shell    │
│   system: String                                                     │
│                                                                       │
│   prompt(input) → Run          emit events, accept control            │
│   complete(input) → RunOutput  consume stream to end, return text     │
└───────────────────────────────┬──────────────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────────────┐
│ ygg-ai                                                               │
│   AiClient::stream(model, request) → ResponseStream → StreamEvent    │
│   types: Message, ToolCall, ToolResult, Usage, etc.                  │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 3. Public API

### `UserInput` + `InputPart`

```rust
/// Ordered user-authored content crossing the agent boundary.
pub struct UserInput {
    pub parts: Vec<InputPart>,
}

pub enum InputPart {
    Text(String),
    Media(ygg_ai::Media),
}

impl From<String> for UserInput; // one Text part
impl From<&str> for UserInput;   // one Text part
impl From<Vec<InputPart>> for UserInput;
```

`UserInput` preserves text/media ordering and maps one-to-one onto
`ygg_ai::UserPart` when persisted. `text_summary()` joins text parts and renders
media as `[image]` / `[audio]` for steering-delivery events and logs. Existing
text-only callers remain source-compatible through the `From<String>` and
`From<&str>` implementations.

This is the second sanctioned widening of the frozen v1 agent boundary. The
text-only signature was an omission: `ygg-ai` already supported media. The
change was approved in the 2026-07-15 native multimodal-input design spec.

### `Agent`

```rust
pub struct Agent {
    client: AiClient,
    model: Model,
    session: Session,
    extensions: ExtensionHost,
    sandbox: SandboxConfig,
    system: String,
    max_turns: u64,
}

pub struct AgentConfig {
    pub client: AiClient,
    pub model: Model,
    pub session: Session,
    pub system: String,
    pub sandbox: SandboxConfig,
    /// Registered tools + observers (see §5). The startup wiring below always
    /// passed this; the struct previously omitted the field by mistake.
    pub extensions: ExtensionHost,
    /// Maximum model turns per run; exceeding it ends the run with
    /// `FinishReason::MaxTurns`. Required for the MaxTurns guard to be real
    /// rather than an unreachable variant.
    pub max_turns: u64,
    /// Prompt-cache retention (`Short` by default in the coding-agent app;
    /// `None` omits provider cache controls).
    pub cache_retention: CacheRetention,
    /// Optional explicit provider cache-affinity identifier. The agent derives
    /// a stable session-path key when this is `None`.
    pub session_id: Option<String>,
}
```

```rust
impl Agent {
    /// Create a new agent. Loads extensions and builds initial tool definitions.
    pub fn new(config: AgentConfig) -> Result<Self, AgentError>;

    /// Begin a run. Streams events; caller drives the returned `Run`.
    /// Returns `Err` for pre-flight failures (bad session, invalid config).
    pub async fn prompt(&mut self, input: impl Into<UserInput>) -> Result<Run<'_>, AgentError>;

    /// Run to completion, returning the final output.
    pub async fn complete(&mut self, input: impl Into<UserInput>) -> Result<RunOutput, AgentError>;
}
```

### `Run` + `RunControl`

```rust
/// A streaming agent run. Drives the event stream; control is via the
/// separately clonable `RunControl` handle.
pub struct Run<'a> {
    stream: Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>,
    control: RunControl,
}

#[derive(Clone)]
pub struct RunControl {
    tx: mpsc::Sender<Control>,
    /// Level-triggered abort flag: `abort()` is synchronous and must be
    /// reliable even when the control channel is momentarily full, so it sets
    /// a shared flag (checked and awaited by the loop) rather than depending
    /// on channel capacity. `Control::Abort` via the channel is honored too.
    abort: Arc<AbortFlag>,
}

pub enum Control {
    /// Inject ordered text/media input before the next model turn.
    Steer(UserInput),
    /// Queue ordered text/media input for after the current run settles.
    FollowUp(UserInput),
    /// Abort the run at the next safe boundary.
    Abort,
}
```

```rust
impl Run<'_> {
    /// Returns a clonable handle for sending control messages while the
    /// run's event stream is being consumed.
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }
}

impl Stream for Run<'_> {
    type Item = AgentEvent;
    // No Result wrapper — errors produce RunFinished { reason: Failed(...) }
}

impl RunControl {
    pub async fn steer(&self, input: impl Into<UserInput>) -> Result<(), AgentError>;
    pub async fn follow_up(&self, input: impl Into<UserInput>) -> Result<(), AgentError>;
    pub fn abort(&self);
}
```

### `AgentEvent`

```rust
/// All events are non-error. A successfully started run always emits exactly
/// one `RunFinished`, even on failure.
pub enum AgentEvent {
    /// A text or reasoning delta from the model.
    OutputDelta {
        channel: OutputChannel,
        text: String,
    },

    /// All queued steering inputs injected before the next model turn.
    /// Strings are single-line summaries in FIFO delivery order. The batch is
    /// emitted after the preceding turn's tools complete.
    SteeringDelivered {
        messages: Vec<String>,
    },

    /// A tool call was emitted by the model. Execution begins.
    ToolStarted {
        id: ToolCallId,
        name: String,
        args: serde_json::Value,
    },

    /// A tool call completed execution.
    ToolFinished {
        id: ToolCallId,
        result: Result<ToolOutput, ToolError>,
    },

    /// The model finished a turn. Contains the assembled message and
    /// cumulative usage.
    TurnFinished {
        message: AssistantMessage,
        usage: Usage,
    },

    /// The run finished. Always the last event.
    RunFinished {
        head: EntryId,
        reason: FinishReason,
    },
}

pub enum OutputChannel {
    Text,
    Reasoning,
}

pub enum FinishReason {
    Completed,
    Aborted,
    Failed(AgentError),
    MaxTurns,
}
```

### `RunOutput`

```rust
pub struct RunOutput {
    /// Concatenated visible text from all turns.
    pub text: String,
    /// Total token usage across the run.
    pub usage: Usage,
    /// Session entry ID after the run.
    pub head: EntryId,
    /// How the run ended.
    pub reason: FinishReason,
}
```

---

## 4. Session

One concrete append-only JSONL session. No `SessionStore` trait, no repository
pattern, no database.

### Session record format (JSONL)

```jsonl
{"type":"entry","id":"001","parent":null,"value":{"type":"message","content":[...]}}
{"type":"entry","id":"002","parent":"001","value":{"type":"message","content":[...]}}
{"type":"head","id":"002"}
{"type":"entry","id":"003","parent":"002","value":{"type":"message","content":[...]}}
{"type":"entry","id":"004","parent":"003","value":{"type":"compaction","summary":"...","first_kept":"001"}}
{"type":"head","id":"004"}
```

Every line is a `SessionRecord`:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Entry(Entry),
    Head { id: EntryId },
}
```

### Data model

```rust
pub struct Session {
    path: PathBuf,
    entries: Vec<Entry>,
    /// `None` for a freshly created, still-empty session. (Correction: the
    /// original non-optional `EntryId` was unrepresentable for an empty file.)
    head: Option<EntryId>,
}

pub struct Entry {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub value: EntryValue,
}

pub enum EntryValue {
    Message(Message),
    Compaction {
        summary: String,
        /// The oldest entry still kept in full-fidelity context.
        first_kept: EntryId,
    },
    Config {
        model: Option<String>,
        reasoning: Option<String>,
    },
}
```

### Methods

```rust
impl Session {
    /// Creates a new empty session file on disk.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, SessionError>;

    /// Opens an existing session, replaying all entries and setting head
    /// from the last `Head` record.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError>;

    /// Appends an entry and updates the head.
    /// Appends two lines: the entry record, then a head record.
    pub fn append(&mut self, value: EntryValue) -> Result<EntryId, SessionError>;

    /// Changes the head to an existing entry. Appends a head record.
    /// Future appends fork from this point.
    pub fn checkout(&mut self, id: EntryId) -> Result<(), SessionError>;

    /// Manual compaction entry point: appends a Compaction entry. `summary`
    /// is caller-provided text (no automatic LLM call in v0.1); `first_kept`
    /// must be an ancestor of (or equal to) the current head.
    pub fn compact(&mut self, summary: impl Into<String>, first_kept: EntryId)
        -> Result<EntryId, SessionError>;

    /// Returns the current head entry ID (`None` for an empty session).
    pub fn head(&self) -> Option<EntryId>;

    /// Returns all entries in insertion order.
    pub fn entries(&self) -> &[Entry];

    /// Reconstructs the model-visible context from the current head.
    ///
    /// Walks the parent chain from `head`, stopping at compaction boundaries.
    /// Returns messages in chronological order, with compaction summaries
    /// injected in front as synthetic **user** messages. (Correction:
    /// `ygg-ai::Message` has no system variant — the system prompt is a
    /// request-level field — so a "synthetic system message" is
    /// unrepresentable; the summary is a user message prefixed
    /// `[summary of earlier conversation]`.)
    pub fn context(&self) -> Result<Vec<Message>, SessionError>;
}
```

### `context()` reconstruction

```
Given entries: E1, E2, E3, C(E4, first_kept=E2), E5, E6, head=E6
Parent chain: E6 → E5 → C → E3 → E2 → E1

Result:
  [user: "[summary of earlier conversation]\n" + compaction.summary]  ← synthetic
  E2.value (message)
  E3.value (message)
  E5.value (message)
  E6.value (message)
```

The compaction entry's `first_kept` tells `context()` where to resume
full-fidelity context. Everything before that is replaced by the summary.

### Crash-consistency

Tool results are appended immediately after each tool executes:

```
1. assistant message (with calls A, B, C) → append
2. tool result A                      → append
3. tool result B                      → append
4. tool result C                      → append
```

On resume, tool results may be partially present. The agent can detect
which calls have results and skip re-execution. There is an unavoidable
window between a tool's side effect and its result being written — tools
are **at-least-once** after an unclean exit. This is documented, not
silently presented as transactional.

Persistence is **process-crash safe, not power-loss durable**: appends are
plain writes to an append-only file (no fsync), so records survive the
process dying but an OS crash can lose the newest ones. `open()` truncates a
torn final line so recovery leaves the file appendable, and keeps a valid
final record that merely lost its trailing newline.

When building the next provider request, adjacent tool-result entries
are coalesced into one user message.

---

## 5. Extensions

One trait, one registration boundary, two hooks.

```rust
/// Implemented by any extension the agent loads.
pub trait Extension: Send + Sync {
    fn register(&self, host: &mut ExtensionHost);
}
```

```rust
pub struct ExtensionHost {
    tools: Vec<Arc<dyn Tool>>,
    observers: Vec<Arc<dyn EventObserver>>,
}
```

```rust
impl ExtensionHost {
    /// Register a tool. Core tools use this same method.
    pub fn tool(&mut self, tool: impl Tool + 'static);

    /// Register an event observer.
    pub fn observe(&mut self, observer: impl EventObserver + 'static);
}
```

```rust
/// Observes agent events without modifying them.
pub trait EventObserver: Send + Sync {
    fn on_event(&self, event: &AgentEvent);
}
```

Context contribution (injecting instructions into the system prompt) is
deferred until its semantics are clear. Adding `fn context(...)` to
`ExtensionHost` later does not break the `Extension` trait.

### Startup wiring

```rust
// (Correction: there is one registry type, `ExtensionHost` — the earlier
// `Extensions` name was the same thing under a second name.)
let mut extensions = ExtensionHost::new();
extensions.load(&CoreTools);      // registers read, search, edit, exec

// Future: load from filesystem
// for ext in load_extensions("/path/to/extensions")? {
//     extensions.load(&ext);
// }

let agent = Agent::new(AgentConfig {
    extensions,
    // ...
})?;
```

Core tools are not special — `CoreTools` is an `Extension` whose `register()`
calls the same public `tool()` method available to every third-party
extension. This proves extensions are first-class from day one. Duplicate
tool names are rejected deterministically: `tool()` keeps the first
registration and `Agent::new` fails with `AgentError::DuplicateTool`.

---

## 6. Tools

### `Tool` trait

```rust
pub trait Tool: Send + Sync {
    /// Returns the tool definition for provider function-calling.
    fn definition(&self) -> ToolDef;

    /// Executes the tool with parsed arguments.
    /// `Ok`/`Err` determines `ToolResult.is_error`.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}
```

```rust
pub struct ToolOutput {
    /// Compact, line-oriented text optimized for LLM consumption.
    pub text: String,
}

pub struct ToolError {
    pub message: String,
}
```

```rust
pub struct ToolContext<'a> {
    /// Canonicalized workspace root.
    pub workspace: &'a Path,
    /// The sandbox configuration.
    pub sandbox: &'a SandboxConfig,
}
```

### `read`

```
Purpose: Read file contents with line numbers and truncation markers.

Schema:
  path: string (required; relative to workspace, or absolute/`~/…` when external paths are enabled)
  offset?: integer (1-indexed line to start from)
  limit?: integer (max lines to return, default 500)

Output:
  src/foo.rs:120-169/412 hash=a19c…
  <content lines with line numbers>
  next_offset=170 truncated=false

Validates: file exists and is not a directory; workspace-only hosts also validate path containment.
Returns: content with line numbers, hash, next_offset, truncation flag.
```

### `search`

```
Purpose: Search file contents. Defaults to literal matching; model can request
regex via mode field.

Schema:
  query: string (required)
  path?: string (directory to search; relative to workspace, or absolute/`~/…` when external paths are enabled; default: workspace root)
  glob?: string (file pattern filter)
  mode?: "literal" | "regex" (default: literal)
  max_results?: integer (default: 50)

Output:
  7 matches
  src/api.rs:42     pub enum AudioPayload {
  src/chat.rs:118   AudioPayload::Inline(...)
  truncated=false

Implementation: shells out to `rg` with --json for structured output,
reformatted as compact line-oriented text.
```

### `edit`

```
Purpose: Single-operation file mutation. One edit call = one operation.

Schema (tagged enum, serde(tag = "operation")):
  {"operation": "create",  "path": "...", "content": "...",
                            "expected_hash": "a19c…"}
  {"operation": "replace", "path": "...", "old": "...", "new": "...",
                            "expected_hash": "a19c…"}
  {"operation": "delete",  "path": "...", "expected_hash": "a19c…"}

expected_hash is optional on all three operations. When provided, the edit
is rejected if the file's current hash differs.

Output (success):
  ok
  src/api.rs  created hash=fa20…

Output (success, replace):
  ok modified=1
  src/api.rs  +18 -7 hash=77bd…

Output (failure, stale file):
  error stale_file
  src/api.rs  expected hash=77bd… actual=91ca…
  The file has changed since it was last read.

Output (failure, no match):
  error no_match
  src/api.rs
  "old string" not found in file. Did you mean:
      42: similar string near match

Validates: allow_edit=true; workspace-only hosts also validate path containment.
Implements: atomic write via temp file + rename.
```

The `expected_hash` field uses the hash returned by `read`. (Correction —
the earlier create wording was garbled.) On create, when the target already
exists, `expected_hash` gates the overwrite against that existing content:
the write proceeds only if the current file hashes to `expected_hash`
(otherwise `stale_file`); a create of a genuinely new file needs no hash. On
replace and delete, it checks the current file hash; a mismatch means the
file changed since it was last read. The field is always optional — omitting
it accepts last-write-wins.

### `exec`

```
Purpose: Run a program or shell command.

Schema (tagged enum):
  {"mode": "process", "program": "cargo", "args": ["test"], "cwd": "."}
  {"mode": "shell",   "command": "cargo test --workspace", "cwd": "."}

cwd is optional on both modes (defaults to workspace root); it may be absolute or `~/…` when external paths are enabled.

Process mode: structured execution, no shell interpretation.
Shell mode: explicit opt-in, requires allow_shell=true.

Output:
  exit=0 duration=0.82s
  stdout: 38 lines, showing last 20
  <last 20 lines of stdout>
  truncated_stdout=head:0 tail:18 omitted:0

Output (failure):
  exit=1 duration=0.15s
  stderr: 3 lines
  <stderr content>
  truncated_stderr=false

Validates: allow_process (for Process), allow_shell (for Shell).
Enforces: timeout from SandboxConfig, max_output_bytes from SandboxConfig.
PTY mode is deferred — not needed for v0.1 tool execution.
Platform: unix-only in v0.1. Cancellation cleanup relies on unix process
groups (kill(-pgid)); rather than silently downgrading to direct-child-only
kills elsewhere, exec returns a clear unsupported_platform error off unix.
```

---

## 7. Security model / policy

Ygg is a **trusted local agent** (see the repository-root `SECURITY.md`). It
runs as the current OS user and does **not** provide operating-system process
isolation. The struct below is a set of capability and resource settings; the
type name `SandboxConfig` is historical, not a claim of containment.

The workspace remains the base for relative built-in-tool paths and default
working directories. Trusted-local hosts may set `allow_external_paths=true`,
which accepts absolute paths, `~/…`, parent components, and external symlink
targets for `read`, `search`, `edit`, and `exec` cwd. Hosts that need to reduce
accidental path mistakes may leave it false, restoring a workspace-only guard.
Neither setting confines a spawned process: once `allow_process` or
`allow_shell` is granted, the child runs with the full filesystem, network, and
environment access of the current user. Those capabilities are broad "run any
program this user can run" grants, not narrow filesystem permissions.

Concrete struct, no trait. Capability gates and path-resolution settings that
the agent can actually enforce.

```rust
pub struct SandboxConfig {
    /// Base for relative tool paths and default working directories. It does
    /// NOT confine spawned processes — those run with the current user's full
    /// access.
    pub workspace: PathBuf,

    /// Permit absolute, `~/…`, parent, and external-symlink paths in built-in
    /// file tools and exec cwd. False enables the optional workspace-only guard.
    pub allow_external_paths: bool,

    /// Allow the edit tool to mutate files.
    pub allow_edit: bool,

    /// Allow exec in Process mode (structured program + args).
    pub allow_process: bool,

    /// Allow exec in Shell mode (arbitrary shell interpretation).
    /// Must be explicitly enabled — Process mode is not a backdoor.
    pub allow_shell: bool,

    /// Maximum duration for an exec call.
    pub exec_timeout: Duration,

    /// Maximum bytes of tool output before truncation.
    pub max_output_bytes: usize,
}
```

No `allow_network` flag: under the trusted-local-agent model a spawned process
already inherits the user's network access, and a Rust process checking a
boolean before spawning a child cannot prevent that child from opening sockets.
An **optional** OS-containment deployment mode (container, seccomp, network
namespace, Landlock, Seatbelt) is a possible *stronger* future deployment — it
is not required for the trusted-local-agent model to be internally correct, and
Ygg does not ship one today. If such a backend is ever added, a real network
policy could accompany it:

```rust
pub enum NetworkPolicy {
    Inherit,
    Deny,
}
```

---

## 8. Agent loop (procedural)

The loop lives in `Agent::prompt()`. Pseudocode:

```rust
async fn prompt(&mut self, input: impl Into<UserInput>) -> Result<Run<'_>, AgentError> {
    // 1. Append ordered text/media parts as a user message.
    let input = input.into();
    let user_msg = Message::User(UserMessage { content: input.into_user_parts() });
    let entry_id = self.session.append(EntryValue::Message(user_msg.clone()))?;

    // 2. Build tool definitions from extensions
    let tools: Vec<ToolDef> = self.extensions.tools.iter()
        .map(|t| t.definition())
        .collect();

    // 3. Build initial conversation context from session
    let mut messages = self.session.context()?;

    // 4. Setup the control channel + abort flag
    let (control_tx, mut control_rx) = mpsc::channel(8);
    let control = RunControl { tx: control_tx, abort: abort_flag.clone() };

    // 5. Build the run loop as a caller-driven async generator.
    //
    // (Correction: the original sketch used `tokio::spawn` while holding
    // `&mut self.session` — a spawned task requires 'static, so that shape
    // cannot compile. The `&mut self` ownership model is preserved by making
    // the loop an async stream that *borrows* the session and only advances
    // when the caller polls `Run`. `tokio::select!` in the caller still works
    // exactly as shown in §2, and dropping `Run` cancels the in-flight
    // provider stream and any running tool.)
    let session = &mut self.session; // &mut borrow held for the run's lifetime
    let stream = async_stream::stream! {
        // ...run loop below; yields AgentEvent directly, no event channel...
    };

    // 6. Return Run + RunControl to caller
    Ok(Run { stream: Box::pin(stream), control })
}
```

The inner loop:

```rust
async fn run_loop(
    client: AiClient, model: Model,
    system: String, session: &mut Session, sandbox: SandboxConfig,
    tools: Vec<ToolDef>, mut messages: Vec<Message>,
    events: mpsc::Sender<AgentEvent>, control: &mut mpsc::Receiver<Control>,
) -> FinishReason
{
    let tool_map: HashMap<String, &dyn Tool> = /* build from extensions */;
    let mut turn = 0u64;

    loop {
        turn += 1;

        // ── Drain steer into messages ──────────────────
        while let Ok(Control::Steer(input)) = control.try_recv() {
            let msg = user_msg(input.into_user_parts());
            session.append(EntryValue::Message(msg.clone())).ok();
            messages.push(msg);
        }

        // ── Check abort ────────────────────────────────
        if control.try_recv().is_ok_and(|c| matches!(c, Control::Abort)) {
            return FinishReason::Aborted;
        }

        // ── Build and send request ─────────────────────
        let req = Request {
            system: Some(system.clone()),
            messages: messages.clone(),
            tools: tools.clone(),
            tool_choice: ToolChoice::Auto,
            ..Default::default()
        };

        let mut stream = match client.stream(&model, req).await {
            Ok(s) => s,
            Err(e) => return FinishReason::Failed(e.into()),
        };

        // ── Consume provider stream ────────────────────
        let mut assistant_parts = vec![];
        let mut text_buffer = String::new();
        let mut reasoning_buffer = String::new();
        let mut current_channel = None;
        let mut usage = Usage::default();

        while let Some(ev) = stream.next().await {
            let ev = match ev {
                Ok(e) => e,
                Err(e) => return FinishReason::Failed(e.into()),
            };

            match ev {
                StreamEvent::TextStart { .. } => {
                    current_channel = Some(OutputChannel::Text);
                }
                StreamEvent::TextDelta { delta, .. } => {
                    text_buffer.push_str(&delta);
                    let _ = events.send(AgentEvent::OutputDelta {
                        channel: OutputChannel::Text,
                        text: delta,
                    }).await;
                }
                StreamEvent::TextEnd { .. } => {
                    assistant_parts.push(AssistantPart::Text(
                        std::mem::take(&mut text_buffer)
                    ));
                }

                StreamEvent::ReasoningStart { .. } => {
                    current_channel = Some(OutputChannel::Reasoning);
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    reasoning_buffer.push_str(&delta);
                    let _ = events.send(AgentEvent::OutputDelta {
                        channel: OutputChannel::Reasoning,
                        text: delta,
                    }).await;
                }
                StreamEvent::ReasoningEnd { .. } => {
                    // Accumulated into the reasoning part at assembly time
                }

                StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. } => {
                    // Nothing is emitted while arguments stream. (Correction:
                    // `ToolStarted` fires when execution begins — see below —
                    // carrying the fully parsed args, matching its doc comment
                    // "Execution begins". Raw argument deltas are never exposed
                    // and a Null-then-filled two-phase event is avoided.)
                }

                StreamEvent::Usage(u) => usage = u,
                StreamEvent::Finished(resp) => {
                    assistant_parts = resp.message.content;
                    usage = resp.usage;
                    break;
                }
                _ => {}
            }
        }

        // ── Append assistant message ──────────────────
        let assistant = AssistantMessage {
            content: assistant_parts.clone(),
            model: model.spec.id.clone(),
            protocol: model.spec.protocol,
        };
        let msg = Message::Assistant(assistant.clone());
        session.append(EntryValue::Message(msg.clone())).ok();
        messages.push(msg);

        let _ = events.send(AgentEvent::TurnFinished {
            message: assistant.clone(),
            usage,
        }).await;

        // ── Extract tool calls ─────────────────────────
        let calls: Vec<&ToolCall> = assistant_parts.iter()
            .filter_map(|p| match p {
                AssistantPart::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect();

        if calls.is_empty() {
            return FinishReason::Completed;
        }

        // ── Execute tools sequentially ─────────────────
        let mut results: Vec<UserPart> = vec![];

        for tc in &calls {
            // ToolStarted is emitted here, with the parsed arguments, as
            // execution begins.
            let tool = match tool_map.get(&tc.name) {
                Some(t) => t,
                None => {
                    let result = UserPart::ToolResult(ToolResult {
                        tool_call_id: tc.id.clone(),
                        content: vec![ToolResultPart::Text(
                            format!("unknown tool: {}", tc.name)
                        )],
                        is_error: true,
                    });
                    results.push(result);
                    continue;
                }
            };

            let ctx = ToolContext {
                workspace: &sandbox.workspace,
                sandbox: &sandbox,
            };

            let args = tc.arguments_value().unwrap_or_default();
            let result = tool.execute(args, &ctx).await;

            let (text, is_error) = match result {
                Ok(out) => (out.text, false),
                Err(e) => (e.message, true),
            };

            let tr = ToolResult {
                tool_call_id: tc.id.clone(),
                content: vec![ToolResultPart::Text(text.clone())],
                is_error,
            };

            let _ = events.send(AgentEvent::ToolFinished {
                id: tc.id.clone(),
                result: result.map(|o| o).map_err(|e| e),
            }).await;

            // ── Persist each result immediately ────────
            let result_msg = Message::User(UserMessage {
                content: vec![UserPart::ToolResult(tr.clone())],
            });
            session.append(EntryValue::Message(result_msg.clone())).ok();
            results.push(UserPart::ToolResult(tr));
        }

        // ── Batch all results into one message for the model ──
        let results_msg = Message::User(UserMessage { content: results });
        messages.push(results_msg);
    }
}
```

---

## 9. File structure

```
crates/ygg-agent/
├── Cargo.toml
└── src/
    ├── lib.rs           # Agent, Run, RunControl, RunOutput, AgentConfig, AgentError
    ├── agent.rs          # Agent::new(), Agent::prompt(), Agent::complete(), run_loop()
    ├── input.rs          # UserInput and ordered text/media InputPart values
    ├── session.rs        # Session, Entry, EntryId, EntryValue, SessionRecord
    ├── extension.rs      # Extension trait, ExtensionHost, EventObserver
    ├── tool.rs           # Tool trait, ToolContext, ToolOutput, ToolError
    ├── sandbox.rs        # SandboxConfig
    ├── events.rs         # AgentEvent, OutputChannel, FinishReason, Control
    └── tools/
        ├── mod.rs        # Tool registration helpers
        ├── read.rs
        ├── search.rs
        ├── edit.rs       # EditRequest enum: Create | Replace | Delete
        └── exec.rs        # ExecRequest enum: Process | Shell
```

---

## 10. Target scope

**In v0.1:**
- Agent loop with tool execution (read, search, edit, exec)
- Session: create, open, append, checkout, context reconstruction
- Extensions: tool registration + event observers
- Trusted-local built-in paths with optional workspace-only accidental-path guard; capability gates (not an OS sandbox)
- Multimodal `UserInput` for prompt, steering, and follow-up controls
- Steering, follow-up, and abort via `RunControl`
- Persistence at completed-message boundaries

**Deferred to v0.2+:**
- Automatic compaction (entry type exists, but only manual trigger)
- PTY mode for exec
- Optional OS-containment deployment mode (container, seccomp, namespaces, Landlock, Seatbelt) and network policy — a stronger *optional* deployment, not required for the trusted-local-agent model
- Context injection via extensions
- File-system extension loading
- Parallel tool execution for read/search
