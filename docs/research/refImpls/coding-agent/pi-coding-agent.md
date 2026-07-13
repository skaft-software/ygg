# Reference Implementation: Pi Coding Agent

> **Primary behavioral and product-shell reference for `ygg-coding-agent`.**
> Source: `earendil-works/pi` → `packages/coding-agent` v0.80.x

---

## Table of Contents

1. [Reference Role](#reference-role)
2. [Why This Reference Was Selected](#why-this-reference-was-selected)
3. [Architecture Overview](#architecture-overview)
4. [Parts to Study Closely](#parts-to-study-closely)
   - [Application Session Behavior](#application-session-behavior)
   - [Session Persistence and Branching](#session-persistence-and-branching)
   - [Resource Model](#resource-model)
   - [Extension Capability Inventory](#extension-capability-inventory)
   - [User-Facing Workflows](#user-facing-workflows)
5. [Parts Not to Copy Directly](#parts-not-to-copy-directly)
   - [The Central `AgentSession` Shape](#the-central-agentsession-shape)
   - [The Full Extension UI Surface](#the-full-extension-ui-surface)
   - [Legacy Compatibility](#legacy-compatibility)
   - [Pi's Default Agent and Tool Assumptions](#pis-default-agent-and-tool-assumptions)
   - [Product Breadth](#product-breadth)
6. [Source-Level Implementation Notes](#source-level-implementation-notes)
   - [AgentSession: The Central Object](#agentsession-the-central-object)
   - [SessionManager: Persistence and Trees](#sessionmanager-persistence-and-trees)
   - [ResourceLoader: Discovery and Precedence](#resourceloader-discovery-and-precedence)
   - [ExtensionRunner: Lifecycle and Integration](#extensionrunner-lifecycle-and-integration)
   - [Compaction Pipeline](#compaction-pipeline)
   - [Run Modes](#run-modes)
   - [Tool Definitions](#tool-definitions)
7. [Ygg's Original Design Responsibility](#yggs-original-design-responsibility)
8. [Reference Classification](#reference-classification)
9. [Ygg Synthesis](#ygg-synthesis)

---

## Reference Role

**Primary behavioral and product-shell reference for `ygg-coding-agent`.**

Pi's coding-agent package demonstrates how a generic AI layer, generic agent runtime, persistent sessions, coding tools, resources, extensions, authentication, and terminal UI can be assembled into a complete coding-agent product.

Ygg does **not** intend to port this package directly.

Ygg's coding-agent must be designed specifically around:

- Ygg's Pi-inspired Rust `ai` package
- Ygg's Terminus-inspired Rust `agent` package
- A smaller extension architecture
- A local-first garage-tinkerer use case
- Ygg's existing Rust TUI
- Autonomous and interactive operation through the same underlying runtime

---

## Why This Reference Was Selected

Pi's coding-agent provides strong reference behavior for:

| Concern | What Pi Demonstrates |
|---------|---------------------|
| **Interactive coding-agent workflows** | Full REPL-style interaction with model steering, follow-up queuing, abort/retry |
| **Print, JSON, RPC, and interactive run modes** | All modes share a single `AgentSession`; I/O is the only difference |
| **Model selection and authentication UX** | `ModelRegistry` resolves credentials; `ConfigSelector` handles startup with no config |
| **Persistent sessions** | JSONL append-only session files with entry IDs and parent IDs forming a tree |
| **JSONL session history** | One file per session; replayable, greppable, human-readable |
| **Branching and checkout** | `parentId` links on entries allow tree navigation; `checkout()` reconstructs context |
| **Context compaction** | Automatic threshold-based + overflow-triggered + manual compaction with branch summaries |
| **Project instructions** | `AGENTS.md` / `CLAUDE.md` discovered via working-directory ancestry walk |
| **Prompt templates** | Named templates with variable expansion, loaded from global and project locations |
| **Skills** | Reusable prompt fragments injected via `<skill>` XML blocks |
| **Themes** | TOML-based color schemes with light/dark variants; reloadable at runtime |
| **Extensions** | TypeScript plugins contributing tools, commands, keybindings, providers, context, UI |
| **Coding tools** | `read`, `write`, `edit`, `bash`, `grep`, `glob`, `ls` with unified output rendering |
| **Steering and follow-up messages** | Queue user input during active agent runs without interrupting |
| **Session export, import, and resume** | HTML export, JSONL import, session directory scanning for resume |

Pi explicitly presents itself as a **minimal terminal coding harness** with multiple run modes and customization through extensions, skills, prompts, and themes.

---

## Architecture Overview

```
┌────────────────────────────────────────────────────────────────────┐
│                         Run Mode Layer                              │
│                                                                     │
│   interactive-mode.ts    print-mode.ts    rpc-mode.ts               │
│   (TUI, Ink/React)       (stdout)         (JSON-RPC over stdio)     │
│                                                                     │
│   All modes instantiate one AgentSession and consume its events.    │
└──────────────────────────────┬─────────────────────────────────────┘
                               │
┌──────────────────────────────▼─────────────────────────────────────┐
│                         AgentSession                                 │
│   (core/agent-session.ts — 2400+ lines)                             │
│                                                                     │
│   Responsibilities:                                                  │
│   • Agent lifecycle (start, prompt, abort, retry, restart)          │
│   • Event subscription + automatic session persistence              │
│   • Model and thinking-level management                             │
│   • Compaction (manual threshold-based, overflow-triggered)         │
│   • Bash execution coordination                                     │
│   • Session switching and branching                                 │
│   • Tool definition registry                                        │
│   • Steering and follow-up queue management                         │
│   • HTML export                                                     │
└──────┬──────────────┬───────────────┬──────────────┬────────────────┘
       │              │               │              │
       ▼              ▼               ▼              ▼
┌──────────┐  ┌────────────┐  ┌────────────┐  ┌──────────────┐
│ Session  │  │ Resource   │  │ Extension  │  │ Model        │
│ Manager  │  │ Loader     │  │ Runner     │  │ Registry     │
│          │  │            │  │            │  │              │
│ • JSONL  │  │ • Exts     │  │ • Lifecycle│  │ • API key    │
│   append │  │ • Skills   │  │   events   │  │   resolution │
│ • Tree   │  │ • Prompts  │  │ • Tool     │  │ • Model      │
│   nav    │  │ • Themes   │  │   registry │  │   discovery  │
│ • Branch │  │ • Context  │  │ • UI hooks │  │ • OAuth      │
│   mgmt   │  │   files    │  │ • Shutdown │  │              │
└──────────┘  └────────────┘  └────────────┘  └──────────────┘
       │              │               │              │
       └──────────────┴───────────────┴──────────────┘
                               │
┌──────────────────────────────▼─────────────────────────────────────┐
│                     Core Libraries                                   │
│                                                                     │
│   @earendil-works/pi-ai          @earendil-works/pi-agent-core     │
│   (provider abstraction)         (agent runtime: loop, tools,       │
│                                    events, context construction)     │
└─────────────────────────────────────────────────────────────────────┘
```

The `AgentSession` is the **only** object shared across run modes. It does not own I/O — modes attach listeners and drive their own rendering.

---

## Parts to Study Closely

### Application Session Behavior

Pi uses a shared application-level session abstraction across interactive, print, and RPC modes.

**What to study:**

1. **How product modes share the same underlying state.** The `AgentSession` constructor takes an `Agent` (from `@earendil-works/pi-agent-core`) and a `SessionManager`. Modes instantiate one and subscribe to events. Print mode calls `session.prompt()` and awaits `session.waitForSettled()`. Interactive mode additionally binds UI context and keyboard handlers. RPC mode serializes events over JSON-RPC.

2. **How agent events trigger persistence and UI updates.** Every `AgentSessionEvent` (turn start, message delta, tool execution, turn end, compaction, error) is emitted to all listeners. The session persists entries on each event via `SessionManager.appendEntry()`. UI components re-render on relevant events.

3. **How model changes and thinking levels interact with sessions.** `cycleModel()` rotates through available or scoped models. `setThinkingLevel()` persists the change as a `ThinkingLevelChangeEntry` in the session file. The session reconstructs thinking level from the session during resume.

4. **How steering and follow-up messages are delivered.** `session.prompt()` accepts a `streamingBehavior` option: `"steer"` interrupts the current agent run (queues a human message for immediate processing), `"followUp"` queues for after the current turn completes. The agent checks the steer queue at each turn boundary.

5. **How abort, retry, compaction, and continuation are exposed.** `session.abort()` cancels the active agent run. Auto-retry with exponential backoff for retryable errors (rate limits, transient failures). `session.compact()` triggers manual compaction. `session.continueAfterCompaction()` resumes after compaction completes.

6. **How session switching and branching affect the active agent context.** `session.checkout(targetEntryId)` reconstructs the agent context up to that entry, discarding later entries. The UI highlights the checked-out position in the tree.

**For Ygg:** Do not assume that one object needs all of these responsibilities. Preserve the concept of a shared runtime while dividing along clear boundaries.

### Session Persistence and Branching

Pi's session format uses append-only entries with entry IDs and parent IDs, allowing one session file to represent a tree rather than only a linear transcript.

**Session file format (JSONL):**

```jsonl
{"type":"session","version":3,"id":"sess_abc123","timestamp":"2026-07-10T00:00:00.000Z","cwd":"/home/user/project"}
{"type":"message","id":"entry_001","parentId":null,"timestamp":"...","message":{...}}
{"type":"message","id":"entry_002","parentId":"entry_001","timestamp":"...","message":{...}}
{"type":"thinking_level_change","id":"entry_003","parentId":"entry_002","timestamp":"...","thinkingLevel":"high"}
{"type":"model_change","id":"entry_004","parentId":"entry_003","timestamp":"...","provider":"anthropic","modelId":"claude-sonnet-5"}
{"type":"message","id":"entry_005","parentId":"entry_004","timestamp":"...","message":{...}}
{"type":"compaction","id":"entry_006","parentId":"entry_005","timestamp":"...","summary":"...","firstKeptEntryId":"entry_001","tokensBefore":45000}
{"type":"branch_summary","id":"entry_007","parentId":"entry_006","timestamp":"...","fromId":"entry_001","summary":"..."}
```

**Entry types:**

| Entry Type | Purpose |
|-----------|---------|
| `message` | Agent message (user, assistant, tool result) |
| `thinking_level_change` | User changed thinking level |
| `model_change` | User switched model |
| `compaction` | Context was compacted; contains summary and `firstKeptEntryId` |
| `branch_summary` | Generated summary of a branch for context reconstruction |
| `custom` | Extension-owned durable data (not sent to LLM) |
| `custom_message` | Extension-injected context message (IS sent to LLM) |
| `label` | User-defined bookmark/marker on an entry |
| `session_info` | Session metadata (display name, etc.) |

**Key behaviors to study:**

1. **JSONL persistence.** One line per entry, append-only. `appendEntry()` uses `appendFileSync()` for durability. No in-place modification — entries are immutable once written.

2. **Parent-linked entries.** Every entry has a `parentId` referencing the previous entry (or `null` for the root). This creates a DAG that can represent branches.

3. **Active-branch reconstruction.** `buildContextEntries(leafId)` walks parent links from the leaf to the root, collecting entries in order. Compaction entries mark truncation boundaries.

4. **Branch checkout.** `checkout(targetEntryId)` changes the active leaf. Future entries fork from that point. The session file continues appending — both branches coexist.

5. **Session resume.** On startup, `SessionManager` reads the session file, parses all entries, and reconstructs the tree. The last entry becomes the active leaf.

6. **Compaction entries.** When context exceeds thresholds, a compaction entry is written. It contains a `summary` and `firstKeptEntryId` — the oldest entry visible to the model after compaction.

7. **Extension-owned durable data.** `CustomEntry` stores extension state (e.g., artifact indexes, caches). `CustomMessageEntry` injects content into the model's context on resume.

8. **Separation between durable history and model-visible context.** The session file contains everything. `buildContextEntries()` produces only the entries visible to the model — respecting compaction boundaries.

**For Ygg:** Preserve the session-tree concept. Derive your own smaller event and entry vocabulary.

### Resource Model

Pi distinguishes between several resource categories, each with its own loading and precedence rules.

**Resource categories:**

| Category | Type | Loaded From | Precedence |
|----------|------|-------------|------------|
| **Extensions** | Executable TypeScript plugins | Global `~/.pi/extensions/`, project `.pi/extensions/` | Project overrides global |
| **Skills** | Prompt fragments (`.md`) | Global `~/.pi/skills/`, project `.pi/skills/` | Project overrides global |
| **Prompt templates** | Named templates with `{{variable}}` expansion | Global `~/.pi/prompts/`, project `.pi/prompts/` | Project overrides global |
| **Themes** | TOML color schemes | Global `~/.pi/themes/`, project `.pi/themes/` | Project overrides global |
| **Context files** | `AGENTS.md`, `CLAUDE.md` | Global `~/.pi/`, every ancestor directory from cwd to root | Closest to cwd wins |
| **System prompt** | CLI flag or file | `--system-prompt` flag or `--system-prompt-file` | Flag overrides file |
| **Append system prompt** | CLI flag | `--append-system-prompt` (repeatable) | Accumulates |

**Resource loader implementation (`DefaultResourceLoader`):**

The loader is constructed with paths and optional overrides. It searches:

1. **Global agent directory** (`~/.pi/` by default) — extensions, skills, prompts, themes, context files
2. **Project directory** (`.pi/` relative to cwd) — extensions, skills, prompts, themes
3. **Working directory ancestry** — `AGENTS.md` / `CLAUDE.md` discovered by walking up from cwd
4. **Additional paths** — CLI flags for extra extension/skill/prompt/theme directories

The loader supports **override callbacks** for every resource category, enabling SDK consumers to filter, augment, or replace resources:

```typescript
interface DefaultResourceLoaderOptions {
  extensionsOverride?: (base: LoadExtensionsResult) => LoadExtensionsResult;
  skillsOverride?: (base: { skills: Skill[] }) => { skills: Skill[] };
  promptsOverride?: (base: { prompts: PromptTemplate[] }) => { prompts: PromptTemplate[] };
  themesOverride?: (base: { themes: Theme[] }) => { themes: Theme[] };
  agentsFilesOverride?: (base: { agentsFiles: ... }) => { agentsFiles: ... };
  systemPromptOverride?: (base: string | undefined) => string | undefined;
  appendSystemPromptOverride?: (base: string[]) => string[];
}
```

**Context file discovery (`loadProjectContextFiles`):**

```typescript
function loadProjectContextFiles(cwd, agentDir) {
  // 1. Load global context from agentDir (CLAUDE.md, AGENTS.md)
  // 2. Walk from cwd to root, collecting context files
  // 3. Closest to cwd appears last (highest precedence)
  // 4. Deduplicate by path
}
```

Candidates checked at each directory level: `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`.

**For Ygg:** Retain the resource categories. Avoid building package distribution and complex override systems before the basic product works. Start with flat directory scanning and simple precedence.

### Extension Capability Inventory

Pi extensions are TypeScript modules that can observe lifecycle events and contribute capabilities. The extension system is large, but its capability categories are what matter for Ygg's design.

**What extensions can do:**

| Capability | Mechanism | Example |
|-----------|-----------|---------|
| **Register tools** | `registerTool(name, definition, handler)` | Custom bash wrapper, database query tool |
| **Register commands** | `registerCommand(name, handler)` | `/deploy`, `/review` slash commands |
| **Register keybindings** | `registerKeybinding(key, handler)` | Custom keyboard shortcuts |
| **Contribute context** | `getContext(options)` → `string` | Inject project-specific instructions |
| **Observe lifecycle** | Event listeners: `onSessionStart`, `onTurnStart`, `onTurnEnd`, `onMessageStart`, `onMessageEnd`, `onToolExecutionStart`, `onToolExecutionEnd`, `onCompaction`, `onBeforeTree` | Logging, metrics, artifact tracking |
| **Persist session data** | `writeCustomEntry(type, data)` | Store extension state in session file |
| **Contribute UI** | `getUIComponents()` → React components | Custom panels, status bars, overlays |
| **Register auth flows** | Provider registration via `ModelRegistry` | OAuth login for new providers |
| **Modify settings** | `SettingsManager` access | Read/write user configuration |
| **Handle shutdown** | `registerShutdownHandler(fn)` | Cleanup on exit |

**Extension lifecycle events (in order):**

```
session_start
    │
    ├── turn_start ──► message_start ──► message_delta* ──► message_end
    │       │
    │       ├── tool_execution_start ──► tool_execution_update* ──► tool_execution_end
    │       │
    │       └── turn_end
    │
    ├── compaction_start ──► compaction_end
    │
    └── session_shutdown
```

**Extension types:**

- **Inline extensions:** Passed directly to `ResourceLoader` as `extensionFactories`. No file I/O.
- **File extensions:** Loaded from global/project extension directories. Each is a TypeScript/JavaScript module exporting a default factory.
- **SDK extensions:** Registered via `AgentSessionConfig.customTools`. Simpler than full extensions.

**For Ygg:** The size of Pi's extension API is not a target. It is **evidence** of the kinds of extension needs that emerge in a mature coding agent. Begin with the smallest capability set: providers, tools/commands, context, and events. UI extensibility grows when concrete extensions require it.

### User-Facing Workflows

Use Pi as the reference for expected workflows:

| Workflow | Pi Behavior |
|----------|------------|
| **Starting without configuration** | `ConfigSelector` guides through credential setup; env vars checked first |
| **Resolving API keys from environment** | `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc. checked by `ModelRegistry` |
| **Logging into subscription providers** | OAuth flow for Codex, Copilot, Anthropic Pro/Max — browser-based device code flow |
| **Selecting and changing models** | `/model` command or `Ctrl+P` to cycle; scoped via `--models` flag |
| **Starting and resuming sessions** | `--resume` or `--session` flag; session picker TUI with preview |
| **Navigating session history** | `/resume` command, arrow keys in session picker, tree view |
| **Branching from an earlier point** | `/checkout <entryId>` — forks from that entry; previous branch preserved |
| **Compacting context** | `/compact` manual; automatic at token threshold; automatic on overflow error |
| **Reloading resources** | `/reload` or `Ctrl+R` — re-scans extensions, skills, prompts, themes |
| **Interrupting the active run** | `Ctrl+C` → `session.abort()`; agent stops at next safe boundary |
| **Queuing steering while work is in progress** | Type during agent run → queued as steer message; processed immediately |
| **Expanding and collapsing tool output** | TUI renders tool output collapsible; `Enter` toggles |
| **Switching themes** | `/theme <name>` — reloads TOML theme at runtime |

**For Ygg:** These are behavior references, not requirements to reproduce every command or keybinding. The workflows should feel familiar to Pi users but can differ in implementation.

---

## Parts Not to Copy Directly

### The Central `AgentSession` Shape

Pi's `AgentSession` (in `core/agent-session.ts`, ~2400 lines) has accumulated responsibility for:

- Agent lifecycle (start, prompt, abort, retry, restart)
- Persistence (every event triggers `SessionManager.appendEntry()`)
- Model management (select, cycle, resolve credentials)
- Thinking levels (set, persist, reconstruct)
- Compaction (threshold check, trigger, result handling, branch summaries)
- Bash execution (coordinate tool calls with process management)
- Sessions and branching (checkout, switch, tree navigation)
- Resources (access extensions, skills, prompts, themes)
- Extensions (register tools, fire lifecycle events, manage shutdown)
- Tools (definition registry, active/inactive toggling)
- Settings (read/write user configuration)
- Themes (current theme, reload)
- Run-mode integration (abort handler, UI context bindings, shutdown hooks)
- HTML export (session → HTML conversion)
- Steering and follow-up (queue management, turn boundary processing)
- Auto-retry (retryable error detection, exponential backoff)
- Skill block parsing (inject skill content into prompts)

This makes sense in an evolved product but should not be treated as Ygg's starting architecture.

**For Ygg:** Preserve the concept of a shared product runtime. Divide responsibilities along clear boundaries: `SessionRuntime`, `PersistenceManager`, `CompactionEngine`, `ResourceRegistry`, `ExtensionHost` — each with a narrow contract.

### The Full Extension UI Surface

Pi allows extensions to replace or modify substantial portions of the interactive interface:

- **Editor components:** Custom React components for message rendering
- **Headers and footers:** Custom Ink components above/below the main area
- **Overlays:** Modal-like UI elements for extension-specific interactions
- **Widgets:** Side panels, status bars, inline decorations
- **Autocomplete:** Command and prompt autocomplete providers
- **Terminal input:** Custom input handling and key processing
- **Statuses:** Custom status bar items
- **Themes:** Full TOML theme system with light/dark/auto variants

**For Ygg:** Do not reproduce this complete surface initially. Prioritize agent capability extensions (tools, providers, context, events). UI extensibility grows only when concrete extensions require it.

### Legacy Compatibility

**Do not copy:**

- Historical session migrations (v1 → v2 → v3 format upgrades) that Ygg does not need
- Compatibility APIs for older Pi versions
- Deprecated resource formats
- Provider compatibility inherited from older Pi versions (`pi-ai/compat` module)
- Package-manager behavior required by the npm ecosystem
- Aliases maintained for existing users (`/model` → `/models`, old flag names)
- Features whose only purpose is backward compatibility

Ygg starts with a clean slate. Define v1 formats and evolve them deliberately.

### Pi's Default Agent and Tool Assumptions

Pi's default coding experience exposes four tools: `read`, `write`, `edit`, `bash`.

Ygg's tool surface remains an open product decision because its agent runtime is based on Terminus-2 rather than Pi's agent loop. Terminus-2 uses a **mono-tool** design (a single tmux session) rather than discrete tool definitions.

Pi's tools are one profile to test, not an architectural requirement. Ygg may choose:
- Terminus-2's mono-tool approach (raw keystrokes → tmux)
- A hybrid: structured tools backed by terminal execution
- A completely different tool surface

### Product Breadth

Do not treat every mature Pi capability as a v0.1 requirement.

The reference implementation describes the **eventual design space**. It does not determine Ygg's initial scope. Start with: one run mode, one session, basic tools, and grow from there.

---

## Source-Level Implementation Notes

### AgentSession: The Central Object

**File:** `core/agent-session.ts` (~2400 lines)

**Construction:**

```typescript
interface AgentSessionConfig {
  agent: Agent;                      // From @earendil-works/pi-agent-core
  sessionManager: SessionManager;    // Persistence + tree navigation
  settingsManager: SettingsManager;  // User configuration
  cwd: string;                       // Working directory
  scopedModels?: Array<{ model: Model<any>; thinkingLevel?: ThinkingLevel }>;
  resourceLoader: ResourceLoader;    // Extensions, skills, prompts, themes, context
  customTools?: ToolDefinition[];    // SDK-registered tools
  modelRegistry: ModelRegistry;      // API key resolution + model discovery
  initialActiveToolNames?: string[]; // Default: [read, bash, edit, write]
  allowedToolNames?: string[];       // Allowlist
  excludedToolNames?: string[];      // Denylist
  baseToolsOverride?: Record<string, AgentTool>;  // Custom runtime tools
  extensionRunnerRef?: { current?: ExtensionRunner };
  sessionStartEvent?: SessionStartEvent;
}
```

**Key internal state:**

```typescript
class AgentSession {
  private agent: Agent;
  private sessionManager: SessionManager;
  private currentModel: Model<any>;
  private currentThinkingLevel: ThinkingLevel;
  private activeRun: { abort: () => void } | null;
  private steerQueue: string[];       // Immediate-interrupt messages
  private followUpQueue: string[];    // After-turn messages
  private toolDefinitions: Map<string, ToolDefinitionEntry>;
  private extensionRunner: ExtensionRunner;
  private eventListeners: Set<AgentSessionEventListener>;
  private isCompacting: boolean;
  private autoRetry: { attempt: number; maxAttempts: number; delay: number };
}
```

**Event emission + persistence pattern:**

Every state change emits an event AND persists to the session file:

```typescript
private emitAndPersist(event: AgentSessionEvent, entry?: SessionEntry): void {
  if (entry) this.sessionManager.appendEntry(entry);
  for (const listener of this.eventListeners) {
    listener(event);
  }
}
```

**Model cycling:**

```typescript
cycleModel(): ModelCycleResult {
  // If scopedModels provided (--models flag): cycle within those
  // Otherwise: cycle through all available models from ModelRegistry
  // Persists as ModelChangeEntry in session
}
```

**Prompt flow:**

```typescript
async prompt(text: string, options?: PromptOptions): Promise<void> {
  // 1. Expand prompt templates if enabled ({{variable}} substitution)
  // 2. Parse skill blocks (<skill name="..." location="...">)
  // 3. If streamingBehavior === "steer": queue as steer
  // 4. If streamingBehavior === "followUp": queue as followUp
  // 5. Otherwise: start agent run with the message
  // 6. Agent run emits: turn_start → message_start → deltas → tool executions → message_end → turn_end
  // 7. On turn_end: process steer queue, then followUp queue
}
```

**For Ygg:** The `AgentSession` is the reference for what a product coordinator needs to manage. But Ygg should decompose into smaller units.

### SessionManager: Persistence and Trees

**File:** `core/session-manager.ts`

**Current session version:** `3`

**Core interface:**

```typescript
interface SessionManager {
  // Session identity
  getSessionId(): string;
  getCwd(): string;
  getSessionFile(): string;

  // Write
  appendEntry(entry: SessionEntry): void;

  // Read
  getHeader(): SessionHeader;
  getEntries(): SessionEntry[];
  getEntry(id: string): SessionEntry | undefined;
  getLeafId(): string;
  getLeafEntry(): SessionEntry | undefined;

  // Tree navigation
  getBranch(leafId: string): SessionEntry[];
  getTree(): SessionTreeNode[];
  getParent(entryId: string): SessionEntry | undefined;
  getChildren(entryId: string): SessionEntry[];

  // Context reconstruction
  buildContextEntries(leafId?: string): SessionEntry[];

  // Label management
  setLabel(targetId: string, label: string | undefined): void;
  getLabel(targetId: string): string | undefined;

  // Session info
  setSessionInfo(name?: string): void;
  getSessionName(): string | undefined;

  // Leaf management (branching)
  checkout(targetId: string): void;   // Change active leaf → future entries fork
  getCurrentLeafId(): string;
}
```

**Session file structure:**

```
~/.pi/sessions/
└── <session-id>.jsonl
```

Each line is a JSON object. First line is always a `SessionHeader`. Subsequent lines are `SessionEntry` objects with `parentId` links.

**Tree reconstruction:**

```typescript
getTree(): SessionTreeNode[] {
  const entries = this.getEntries();
  const byId = new Map(entries.map(e => [e.id, e]));
  const children = new Map<string, SessionTreeNode[]>();

  for (const entry of entries) {
    const parentId = entry.parentId ?? "__root__";
    if (!children.has(parentId)) children.set(parentId, []);
    children.get(parentId)!.push({ entry, children: [], label: ... });
  }

  // Build tree from root entries
  const roots = children.get("__root__") ?? [];
  const build = (nodes: SessionTreeNode[]) => {
    for (const node of nodes) {
      node.children = children.get(node.entry.id) ?? [];
      build(node.children);
    }
  };
  build(roots);
  return roots;
}
```

**Context reconstruction with compaction awareness:**

```typescript
buildContextEntries(leafId?: string): SessionEntry[] {
  const branch = this.getBranch(leafId ?? this.getLeafId());
  const compactionEntry = getLatestCompactionEntry(branch);

  if (!compactionEntry) return branch;

  // Keep only entries after firstKeptEntryId
  const keptEntries = branch.filter(e =>
    e.timestamp >= compactionEntry.timestamp ||
    e.id === compactionEntry.firstKeptEntryId
  );

  return keptEntries;
}
```

**For Ygg:** The JSONL + parentId tree format is the key design to adopt. It's simple, append-only, and supports branching naturally. The `buildContextEntries()` logic is the critical path — it determines what the model sees.

### ResourceLoader: Discovery and Precedence

**File:** `core/resource-loader.ts` + `DefaultResourceLoader` class

**Discovery algorithm:**

```
loadExtensions():
  1. Scan ~/.pi/extensions/ for .ts/.js files → global extensions
  2. Scan .pi/extensions/ (relative to cwd) → project extensions
  3. Merge: project overrides global (by name)
  4. Scan additionalExtensionPaths
  5. Add extensionFactories (inline)
  6. Apply extensionsOverride if provided

loadSkills():
  1. Scan ~/.pi/skills/ for .md files → global skills
  2. Scan .pi/skills/ → project skills
  3. Merge: project overrides global (by name)
  4. Apply skillsOverride

Same pattern for prompts/, themes/
```

**Context file discovery:**

```typescript
loadProjectContextFiles(cwd, agentDir):
  1. Load from agentDir (global) → AGENTS.md, CLAUDE.md
  2. Walk cwd → root, collecting context files
  3. Reverse order: closest to cwd = last = highest precedence
  4. Deduplicate by canonical path
```

**System prompt building:**

```typescript
buildSystemPrompt(options: BuildSystemPromptOptions): string {
  // 1. Base system prompt (from template or --system-prompt)
  // 2. Append context files (AGENTS.md)
  // 3. Append skills (active skills injected)
  // 4. Append project instructions
  // 5. Append --append-system-prompt values
  // 6. Wrap in <project_context> tags
}
```

**For Ygg:** The discovery pattern (global → project → ancestry walk) is the reference. The override system can be much simpler initially — just directory scanning with name-based precedence.

### ExtensionRunner: Lifecycle and Integration

**File:** `core/extensions/runner.ts`

Extensions are loaded by `ExtensionLoader` and wrapped by `ExtensionRunner` for lifecycle management.

**Lifecycle:**

```
loadExtensions() → createExtensionRuntime() for each
    │
    ▼
bind(AgentSession):
    ├── Register tools via session.registerTool()
    ├── Register commands via session.registerCommand()
    ├── Register keybindings via session.registerKeybinding()
    ├── Subscribe to events:
    │     ├── onSessionStart
    │     ├── onTurnStart / onTurnEnd
    │     ├── onMessageStart / onMessageEnd
    │     ├── onToolExecutionStart / onToolExecutionEnd
    │     └── onCompaction
    └── Register shutdown handler
    │
    ▼
AgentSession.run():
    ├── Emit session_start → extensions receive
    ├── Loop: turn_start → message → tool_executions → turn_end
    │     └── Extensions observe, contribute context, modify behavior
    ├── On compaction: extensions can contribute summary details
    └── On shutdown: extensions clean up
```

**Extension definition:**

```typescript
interface Extension {
  name: string;
  version: string;
  description?: string;
  register(runtime: ExtensionRuntime): void | Promise<void>;
}

interface ExtensionRuntime {
  // Tool registration
  registerTool(name: string, definition: ToolDefinition, handler: ToolHandler): void;

  // Command registration
  registerCommand(name: string, handler: CommandHandler): void;

  // Keybinding registration
  registerKeybinding(key: string, handler: () => void): void;

  // Context contribution
  getContext?: (options: GetContextOptions) => string | Promise<string>;

  // Lifecycle events
  onSessionStart?: (event: SessionStartEvent) => void;
  onTurnStart?: (event: TurnStartEvent) => void;
  onTurnEnd?: (event: TurnEndEvent) => void;
  onMessageStart?: (event: MessageStartEvent) => void;
  onMessageEnd?: (event: MessageEndEvent) => void;
  onToolExecutionStart?: (event: ToolExecutionStartEvent) => void;
  onToolExecutionEnd?: (event: ToolExecutionEndEvent) => void;
  onCompaction?: (event: BeforeCompactResult) => void;
  onBeforeTree?: (event: SessionBeforeTreeResult) => void;

  // Session data persistence
  writeCustomEntry(type: string, data: unknown): void;

  // UI
  getUIComponents?: () => UIComponent[];
  getHeader?: () => InkComponent;
  getFooter?: () => InkComponent;

  // Shutdown
  registerShutdownHandler(handler: () => void | Promise<void>): void;
}
```

**For Ygg:** The lifecycle event list is the reference for what hooks extensions need. Start with `sessionStart`, `turnStart`/`turnEnd`, `toolExecution`, and `shutdown`. Add UI and compaction hooks later.

### Compaction Pipeline

**Files:** `core/compaction/`

**Three triggers:**

1. **Threshold-based (proactive):** Token count exceeds `compact_threshold` → compact automatically
2. **Overflow-triggered (reactive):** `ContextLengthExceededError` from model → compact and retry
3. **Manual:** User runs `/compact` command

**Pipeline:**

```
shouldCompact(contextTokens, threshold) → boolean
    │
    ▼
prepareCompaction(messages, model, contextWindow):
    │
    ├── Identify messages to keep (system + recent)
    ├── Identify messages to summarize (older middle portion)
    └── Return: { toKeep, toSummarize, firstKeptEntryId }
    │
    ▼
compact(toKeep, toSummarize, agent, model):
    │
    ├── Ask current model to summarize the toSummarize messages
    ├── Combine: toKeep + summary
    ├── Create CompactionEntry in session file
    └── Return: CompactionResult { summary, firstKeptEntryId, tokensBefore }
    │
    ▼
generateBranchSummary(messages, agent, model):
    │
    └── Optional: generate a longer summary for branch context
    │   (used when resuming from a compacted session)
```

**Compaction entry in session:**

```typescript
interface CompactionEntry {
  type: "compaction";
  summary: string;          // LLM-generated summary of truncated messages
  firstKeptEntryId: string; // Oldest entry still visible to the model
  tokensBefore: number;     // Token count that triggered compaction
  details?: unknown;        // Extension-specific data
  fromHook?: boolean;       // Whether an extension generated this compaction
}
```

**For Ygg:** The compaction trigger taxonomy (threshold, overflow, manual) and the entry format are the references. Compare with Terminus-2's 3-step summarization approach (summary → questions → answers). Ygg may adopt either or synthesize a new approach.

### Run Modes

**Three modes, one session:**

| Mode | File | I/O | Use Case |
|------|------|-----|----------|
| **Interactive** | `modes/interactive/interactive-mode.ts` | TUI via Ink/React | Default human-facing mode |
| **Print** | `modes/print-mode.ts` | stdin → stdout | Scripting, pipes, CI |
| **RPC** | `modes/rpc/rpc-mode.ts` | JSON-RPC over stdio | Editor integration, external tools |

**Interactive mode:**

- Uses [Ink](https://github.com/vadimdemedes/ink) (React for terminals) for rendering
- Components: message list, input area, status bar, tool output panels, model picker
- Theme system with TOML-based color schemes
- Keyboard shortcuts via custom keybinding handler
- Session picker TUI for resume

**Print mode:**

- Reads from stdin (or `-p "prompt"` flag)
- Streams agent output to stdout
- Exits when agent settles (no more turns to process)
- No interactive UI — suitable for `git diff | pi --print`

**RPC mode:**

- Listens on stdin for JSON-RPC 2.0 requests
- Methods: `prompt`, `abort`, `setModel`, `setThinkingLevel`, `compact`, `checkout`, etc.
- Streams events as JSON-RPC notifications
- No rendering — client handles all UI

**For Ygg:** The mode separation is the reference. All modes should share one runtime. Ygg's TUI is already in Rust — the interactive mode would use Ygg's existing TUI rather than Ink.

### Tool Definitions

**Files:** `core/tools/`

**Built-in tools:**

| Tool | File | Description |
|------|------|-------------|
| `read` | `read.ts` | Read file contents with line numbers and syntax highlighting |
| `write` | `write.ts` | Create or overwrite a file |
| `edit` | `edit.ts` | String-replace edit within a file |
| `bash` | `bash.ts` | Execute shell commands with timeout, output capture, process management |
| `grep` | `grep.ts` | Search file contents with regex |
| `glob` | `glob.ts` | Find files matching glob patterns |
| `ls` | `ls.ts` | List directory contents |

**Tool definition format:**

```typescript
interface ToolDefinition {
  name: string;
  description: string;
  parameters: TSchema;              // TypeBox schema
  handler: ToolHandler;
  renderOutput?: ToolOutputRenderer; // For TUI rendering
  requiresApproval?: boolean;
  isReadOnly?: boolean;
}

interface ToolHandler {
  (input: Record<string, unknown>, context: ToolContext): Promise<ToolResult>;
}

interface ToolResult {
  content: (TextContent | ImageContent)[];
  isError?: boolean;
}

interface ToolContext {
  signal: AbortSignal;
  session: AgentSession;           // Access to session state
  cwd: string;
}
```

**For Ygg:** The tool definition format is the reference. Ygg's tool surface may differ (mono-tool vs discrete tools) but the registration pattern (name + description + schema + handler) is universal.

---

## Ygg's Original Design Responsibility

No reference implementation directly represents the desired Ygg coding-agent.

Ygg must design its own simple product coordinator that connects:

```
AI providers and authentication
            ↓
Terminus-style agent execution
            ↓
coding tools and terminal environment
            ↓
durable branching sessions
            ↓
interactive or autonomous frontend
```

**Decisions this product layer must make:**

| Decision | Context |
|----------|---------|
| How a generic Ygg agent becomes a coding agent | System prompt, tool set, project context injection |
| Which tool profile is used | Terminus-2 mono-tool (tmux), Pi-style discrete tools, or hybrid |
| How autonomous completion is confirmed | Terminus-2 uses double-confirmation; Pi uses explicit `/complete` |
| How terminal state is represented | Tmux pane capture (Terminus-2) vs incremental output (Pi bash tool) |
| How agent events become session entries | Event → entry mapping; what granularity to persist |
| How sessions reconstruct agent context | Walk parent links; apply compaction boundaries; build context |
| How context compaction interacts with durable history | Summary entries + `firstKeptEntryId` markers |
| How users steer an active autonomous loop | Steer queue (interrupt) + followUp queue (wait) |
| How extensions contribute behavior | Tool registration, context injection, lifecycle hooks |
| How all run modes share one product runtime | Single `SessionRuntime` with mode-specific I/O adapters |

These decisions should be derived from Ygg's own requirements and validated against reference implementations and benchmarks.

---

## Reference Classification

Pi coding-agent should be used as:

| Reference Category | Use |
|-------------------|-----|
| Product behavior | **Primary reference** |
| Session semantics | **Primary reference** |
| User experience | **Primary reference** |
| Resource taxonomy | **Primary reference** |
| Extension use cases | **Primary reference** |
| Internal architecture | Selective reference |
| Agent loop | Not the primary reference |
| Tool surface | One candidate |
| Rust structure | Not applicable |
| Initial implementation scope | Not a reference |

---

## Ygg Synthesis

The intended relationship is:

```
Pi AI
    provides the provider and model design reference

Terminus-2
    provides the autonomous agent-loop reference

Pi coding-agent
    provides the product behavior and session reference

Ygg coding-agent
    provides the original minimal synthesis
```

Ygg should aim to reproduce the useful **properties** of Pi's coding-agent, not its accumulated internal complexity.

**Architectural rule:**

> **Reference Pi to discover what the product must make possible. Do not begin from Pi's classes and translate them into Rust.**

---

## Cross-References

| Concern | Related Reference Documents |
|---------|---------------------------|
| Provider/model design | [PI Unified AI Provider](../ai/pi-ai.md) |
| Agent execution loop | [Terminus-2 Implementation](../agent/terminus-2.md) |
| Bash execution subsystem | [Codex Tools Reference](./codex-tools.md) |
| API wire protocols | [`../apidocs/`](../../apidocs/) — OpenAI Completions, OpenAI Responses, Anthropic Messages |
