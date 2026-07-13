# Reference Implementations — Research Index

> **`docs/research/refImpls/`** — Deep-dive documentation of how key agent and AI-infrastructure systems are implemented in practice. These are source-level analyses, not tutorials.

---

## Directory Structure

```
refImpls/
├── README.md                           # This index
├── ai/
│   └── pi-ai.md                        # PI unified AI provider (30+ providers, 9 wire protocols)
├── agent/
│   └── terminus-2.md                   # Harbor's Terminus-2 autonomous agent
├── coding-agent/
│   ├── pi-coding-agent.md              # Pi coding-agent product shell (primary Ygg reference)
│   └── codex-tools.md                  # Shell execution, file patching, permissions, autonomous CLI
└── tui/
    └── pi-tui-rust-port.md             # Pi TUI behaviors for Ygg's Rust TUI
```

---

## Documents

| Document | System | Version | Role |
|----------|--------|---------|------|
| [`ai/pi-ai.md`](./ai/pi-ai.md) | PI `@earendil-works/pi-ai` | v0.80.x | Provider/model design reference |
| [`agent/terminus-2.md`](./agent/terminus-2.md) | Harbor's Terminus-2 | Research preview | Autonomous agent-loop reference |
| [`coding-agent/pi-coding-agent.md`](./coding-agent/pi-coding-agent.md) | PI `packages/coding-agent` | v0.80.x | **Primary product-shell reference for Ygg** |
| [`coding-agent/codex-tools.md`](./coding-agent/codex-tools.md) | OpenAI Codex CLI patterns | — | Tool subsystem reference |
| [`tui/pi-tui-rust-port.md`](./tui/pi-tui-rust-port.md) | Pi TUI (Ink/React) | — | TUI behavior reference for Ygg's Rust TUI |

---

## What These Documents Are

Each `refImpl` document is a **source-level reverse-engineering** of a production system. They trace every code path, data structure, and design decision. The goal is to understand not just _what_ the system does, but _how_ and _why_ it works the way it does — at a granularity sufficient to reimplement, fork, or critique the approach.

Documents are cross-referenced with the upstream API specifications in [`../apidocs/`](../apidocs/) to ground implementation decisions in the wire protocols they abstract over.

---

## Relationship to API Documentation

```
docs/research/
├── apidocs/                         # Upstream API specifications (OpenAI, Anthropic)
│   ├── anthropic-messages/          # Anthropic Messages API (POST /v1/messages)
│   ├── openai-responses/            # OpenAI Responses API (POST /responses)
│   └── compatibility/               # OpenAI Chat Completions API (POST /v1/chat/completions)
│       └── openAI-chat-completions/
│
└── refImpls/                        # THIS DIRECTORY — how systems consume those APIs
    ├── ai/                          # Provider/client abstraction layer
    ├── agent/                       # Autonomous agent runtimes
    ├── coding-agent/                # Full product shells (agent + tools + sessions + UX)
    └── tui/                         # Terminal UI behaviors
```

**Direction of dependency:** `refImpls/` → `apidocs/`

- `agent/terminus-2.md` references the Chat Completions and Messages APIs that Terminus-2 calls directly via LiteLLM
- `ai/pi-ai.md` references every API it abstracts — Completions, Responses, Messages, Generative AI, Vertex, Mistral, Bedrock
- `coding-agent/pi-coding-agent.md` references both the AI layer and the agent layer, plus all tool-level API interactions

---

## System Comparison

| Dimension | Terminus-2 | PI Unified AI Provider | Pi Coding Agent |
|-----------|-----------|----------------------|-----------------|
| **What it is** | Autonomous terminal agent | Multi-provider LLM client library | Full coding-agent product shell |
| **Language** | Python | TypeScript | TypeScript |
| **Scope** | Single agent loop with tmux | 30+ providers, 9 wire protocols | Agent + tools + sessions + resources + extensions + TUI |
| **Key abstraction** | Keystroke commands → tmux session | Unified `Context` + `AssistantMessageEventStream` | `AgentSession` coordinating all subsystems |
| **Auth model** | Single API key via LiteLLM | Declarative `ProviderAuth` (API key + OAuth) | `ModelRegistry` + OAuth flows + env vars |
| **Streaming** | Blocking `chat.chat()` call | Async-iterable `EventStream` with typed events | Events through `AgentSession` to mode-specific renderers |
| **Error handling** | 3-tier fallback chain (unwind → summarize → fallback → ultimate) | Stream-level error events + `ModelsError` typed exceptions | Auto-retry + compaction recovery + error events |
| **Context management** | 3-step subagent summarization | Client-side `Context.messages[]` array; no server-side state | Threshold/overflow/manual compaction + branch summaries |
| **Sessions** | Single linear run | N/A | JSONL append-only with parentId tree + branching |
| **Cross-model** | N/A (single model per run) | `transformMessages()` for cross-provider handoff | Model cycling + scoped model lists |
| **Extensions** | None | Custom providers via `createProvider()` | Full extension system (tools, commands, keybindings, context, UI, lifecycle) |
| **Resources** | Prompt templates only | N/A | Extensions, skills, prompts, themes, context files |
| **RL support** | Rollout details (token IDs + logprobs) | Not applicable | Not applicable |
| **Trajectory format** | ATIF (Agent Trajectory Interchange Format) | Not applicable | JSONL session entries |

---

## Key Architectural Patterns

### 1. Unified Abstraction Over Heterogeneous Backends

Terminus-2 abstracts over OpenAI and Anthropic via LiteLLM. PI abstracts over 30+ providers via a `Provider` interface + per-API implementation modules. Both use a **common internal type** (`Command` in Terminus-2, `AssistantMessageEvent` in PI) that all backends must produce.

### 2. Lazy Loading / Late Binding

Terminus-2 selects its parser (JSON or XML) at construction time. PI lazy-loads entire API implementations via dynamic `import()`. Pi Coding Agent lazy-loads extensions and resources. Late binding keeps cold-start fast and bundles small.

### 3. Error Recovery Chains

Terminus-2's 3-step summarization with 4-tier fallback is a sophisticated recovery pattern. Pi Coding Agent uses auto-retry with exponential backoff for retryable errors, plus compaction recovery for context overflow.

### 4. Context Compression

Terminus-2 compresses via LLM-powered summarization (summary → questions → answers → compress). Pi Coding Agent compresses via a similar compaction pipeline but integrates it into the session tree with `CompactionEntry` markers and `firstKeptEntryId` boundaries.

### 5. Session as Tree

Pi Coding Agent's JSONL session format with `parentId` links is the most sophisticated session model across all three systems. It enables branching, checkout, compaction-aware context reconstruction, and extension-owned durable data — all from a single append-only file.

### 6. Declarative Configuration

All three systems favor declarative configuration over imperative setup: Terminus-2's `AgentConfig`, PI's `createProvider()` + `createModels()`, and Pi Coding Agent's `AgentSessionConfig` + `ResourceLoader` + `SettingsManager`.

---

## Ygg Synthesis

The intended relationship between these references and Ygg:

```
Pi AI (ai/pi-ai.md)
    → provides the provider and model design reference

Terminus-2 (agent/terminus-2.md)
    → provides the autonomous agent-loop reference

Pi Coding Agent (coding-agent/pi-coding-agent.md)
    → provides the product behavior and session reference

Codex Tools (coding-agent/codex-tools.md)
    → provides the tool subsystem reference

Pi TUI (tui/pi-tui-rust-port.md)
    → provides the interactive surface reference

Ygg coding-agent
    → provides the original minimal synthesis
```

**Architectural rule:**

> **Reference Pi to discover what the product must make possible. Do not begin from Pi's classes and translate them into Rust.**

---

## Reading Order

**If you're building the Ygg coding-agent product shell:** Start with `coding-agent/pi-coding-agent.md`. It inventories every product behavior Ygg needs to support. Then read the "Parts Not to Copy Directly" section to understand what to avoid. Finally, cross-reference the agent execution model in `agent/terminus-2.md` and the provider layer in `ai/pi-ai.md`.

**If you're building the agent execution core:** Start with `agent/terminus-2.md`. It covers the full agent lifecycle — prompt construction, tool execution, error recovery, task completion, and trajectory recording. Then read the relevant API docs in `../apidocs/` for the provider you're targeting.

**If you're building the provider/client layer:** Start with `ai/pi-ai.md`. It covers the unified type system, provider model, auth subsystem, streaming architecture, message transformation, and cost tracking.

**If you're designing tools:** Start with `coding-agent/codex-tools.md` for shell execution, file patching, permissions, and autonomous CLI patterns.

**If you're extending the TUI:** Start with `tui/pi-tui-rust-port.md` for the interactive surface behaviors Ygg's Rust TUI should support.

---

## Updating These Documents

When the upstream systems change:

1. **Terminus-2:** Re-read the `terminal_bench/agents/terminus_2/` source in Harbor. Check `_run_agent_loop`, `_query_llm`, `_summarize`, and `_execute_commands`.

2. **PI AI:** Re-read `packages/ai/src/` in the PI repo. Check `types.ts`, `models.ts`, each provider factory, and each API implementation. Check `CHANGELOG.md` for changes.

3. **Pi Coding Agent:** Re-read `packages/coding-agent/src/core/` in the PI repo. Check `agent-session.ts`, `session-manager.ts`, `resource-loader.ts`, and the compaction and extension subsystems.

4. **Cross-references:** When upstream API docs change (new endpoints, new parameters, new modalities), update both the `apidocs/` directory and the relevant sections in the refImpl documents.
