# PI Unified AI Provider — Reference Implementation

> **`@earendil-works/pi-ai` v0.80.x** — Unified LLM API with 30+ provider integrations, automatic auth resolution, token and cost tracking, context serialization, and cross-provider handoffs.

**Source:** <https://github.com/earendil-works/pi/tree/main/packages/ai>

---

## Table of Contents

1. [Overview](#overview)
2. [Architecture](#architecture)
   - [Layered Design](#layered-design)
   - [Core Data Types](#core-data-types)
   - [Unified Event Protocol](#unified-event-protocol)
3. [Models Collection](#models-collection)
   - [`Models` Interface](#models-interface)
   - [`createModels()` Implementation](#createmodels-implementation)
   - [Auth Resolution Flow](#auth-resolution-flow)
4. [Provider System](#provider-system)
   - [Provider Interface](#provider-interface)
   - [`createProvider()`](#createprovider)
   - [Built-in Provider Factories](#built-in-provider-factories)
5. [API Implementations](#api-implementations)
   - [The `ProviderStreams` Contract](#the-providerstreams-contract)
   - [Lazy Loading Architecture](#lazy-loading-architecture)
   - [OpenAI Completions API](#openai-completions-api)
   - [OpenAI Responses API](#openai-responses-api)
   - [Anthropic Messages API](#anthropic-messages-api)
6. [Stream & Event System](#stream--event-system)
   - [`EventStream<T, R>`](#eventstreamt-r)
   - [`AssistantMessageEventStream`](#assistantmessageeventstream)
   - [Event Lifecycle](#event-lifecycle)
   - [Lazy Streams (`lazyStream`)](#lazy-streams-lazystream)
7. [Auth Subsystem](#auth-subsystem)
   - [Auth Types](#auth-types)
   - [Credential Store](#credential-store)
   - [API Key Auth](#api-key-auth)
   - [OAuth Auth](#oauth-auth)
   - [Lazy OAuth](#lazy-oauth)
   - [Resolution Pipeline](#resolution-pipeline)
8. [Context & Message Model](#context--message-model)
   - [Unified Message Types](#unified-message-types)
   - [Content Blocks](#content-blocks)
   - [Context Serialization](#context-serialization)
9. [Cross-Provider Message Transformation](#cross-provider-message-transformation)
   - [`transformMessages()`](#transformmessages)
   - [Tool Call ID Normalization](#tool-call-id-normalization)
   - [Thinking Block Downgrading](#thinking-block-downgrading)
   - [Image Placeholding](#image-placeholding)
   - [Orphaned Tool Call Handling](#orphaned-tool-call-handling)
10. [Streaming Modes](#streaming-modes)
    - [`stream` vs `streamSimple`](#stream-vs-streamsimple)
    - [Thinking/Reasoning in `streamSimple`](#thinkingreasoning-in-streamsimple)
    - [Token Budget Management](#token-budget-management)
11. [Cost & Usage Tracking](#cost--usage-tracking)
    - [Tiered Pricing](#tiered-pricing)
    - [Service Tier Multipliers](#service-tier-multipliers)
    - [Prompt Caching Costs](#prompt-caching-costs)
12. [Model Catalog](#model-catalog)
    - [Static Model Definitions](#static-model-definitions)
    - [Dynamic Providers (`refreshModels`)](#dynamic-providers-refreshmodels)
    - [Model Capabilities (`input`)](#model-capabilities-input)
    - [Thinking Level Maps](#thinking-level-maps)
13. [Image Generation](#image-generation)
14. [Custom Providers & OpenAI Compatibility](#custom-providers--openai-compatibility)
    - [`createProvider()` for Custom Endpoints](#createprovider-for-custom-endpoints)
    - [OpenAI Compatibility Settings](#openai-compatibility-settings)
15. [Capability Gap Analysis: What PI Lacks](#capability-gap-analysis-what-pi-lacks)
    - [Audio Input/Output](#audio-inputoutput)
    - [Realtime Sessions](#realtime-sessions)
    - [Structured Outputs (`json_schema`)](#structured-outputs-json_schema)
    - [Conversation State (Store/Retrieve)](#conversation-state-storeretrieve)
    - [Web Search / File Search / Code Interpreter Tools](#web-search--file-search--code-interpreter-tools)
    - [Computer Use / Browser Use](#computer-use--browser-use)
    - [Background Responses](#background-responses)
    - [Mid-Conversation System Messages](#mid-conversation-system-messages)
16. [Cross-References to API Documentation](#cross-references-to-api-documentation)

---

## Overview

PI's `@earendil-works/pi-ai` is a TypeScript library providing a **unified interface** to 30+ LLM providers. Key design principles:

| Principle | Implementation |
|-----------|---------------|
| **Unified types** | Single `Message`, `Context`, `AssistantMessage` types regardless of provider |
| **Provider-agnostic streaming** | Every provider returns an `AssistantMessageEventStream` with identical event shapes |
| **Lazy loading** | API implementations and OAuth flows are dynamically imported to keep bundle size small |
| **Auth as data** | Auth is declarative on `Provider` objects; `Models` resolves it before each request |
| **Cross-provider handoff** | `transformMessages()` normalizes message history when switching providers mid-session |
| **Cost tracking** | Per-model tiered pricing with service-tier multipliers and prompt-cache cost breakdowns |
| **Tool calling only** | Library deliberately only includes models that support tool/function calling (agentic workflows) |

---

## Architecture

### Layered Design

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Application Code                            │
│                                                                     │
│   models.stream(model, context, options)                            │
│   models.complete(model, context, options)                          │
│   models.streamSimple(model, context, options)                      │
│   models.completeSimple(model, context, options)                    │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────────────┐
│                       Models Collection                             │
│                                                                     │
│   • Provider registry (setProvider / deleteProvider)                │
│   • Model lookup (getModel / getModels)                             │
│   • Auth resolution (getAuth → resolveProviderAuth)                 │
│   • Stream delegation + auth injection (applyAuth)                  │
│   • Credential store (InMemoryCredentialStore)                      │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────────────┐
│                         Provider Objects                             │
│                                                                     │
│   • id, name, baseUrl, headers                                      │
│   • auth (ProviderAuth: apiKey + optional oauth)                    │
│   • getModels() → Model[]                                           │
│   • refreshModels()? → dynamic fetch                                │
│   • stream / streamSimple → dispatch to API implementation          │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────────────┐
│                      API Implementations                             │
│   (one module per wire protocol, lazily loaded)                     │
│                                                                     │
│   openai-completions.ts      anthropic-messages.ts                  │
│   openai-responses.ts        google-generative-ai.ts                │
│   mistral-conversations.ts   bedrock-converse-stream.ts             │
│   azure-openai-responses.ts  google-vertex.ts                       │
│   openai-codex-responses.ts                                         │
│                                                                     │
│   Each exports: { stream, streamSimple }                            │
│   Each returns: AssistantMessageEventStream                         │
└─────────────────────────────────────────────────────────────────────┘
```

### Core Data Types

All types are defined in `src/types.ts`. The system is built around a few central structures:

```typescript
// === Content Blocks ===
interface TextContent    { type: "text";    text: string;            textSignature?: string }
interface ThinkingContent { type: "thinking"; thinking: string;      thinkingSignature?: string; redacted?: boolean }
interface ImageContent   { type: "image";   data: string;            mimeType: string }
interface ToolCall       { type: "toolCall"; id: string;             name: string; arguments: Record<string, any>; thoughtSignature?: string }

// === Messages ===
interface UserMessage      { role: "user";      content: string | (TextContent | ImageContent)[]; timestamp: number }
interface AssistantMessage { role: "assistant"; content: (TextContent | ThinkingContent | ToolCall)[]; api: Api; provider: ProviderId; model: string; usage: Usage; stopReason: StopReason; errorMessage?: string; timestamp: number; ... }
interface ToolResultMessage { role: "toolResult"; toolCallId: string; toolName: string; content: (TextContent | ImageContent)[]; isError: boolean; timestamp: number }

// === Context ===
interface Context { systemPrompt?: string; messages: Message[]; tools?: Tool[] }

// === Usage & Cost ===
interface Usage { input: number; output: number; cacheRead: number; cacheWrite: number; cacheWrite1h?: number; reasoning?: number; totalTokens: number; cost: { input: number; output: number; cacheRead: number; cacheWrite: number; total: number } }

// === Stop Reasons ===
type StopReason = "stop" | "length" | "toolUse" | "error" | "aborted";

// === APIs ===
type KnownApi = "openai-completions" | "mistral-conversations" | "openai-responses" | "azure-openai-responses" | "openai-codex-responses" | "anthropic-messages" | "bedrock-converse-stream" | "google-generative-ai" | "google-vertex";
type Api = KnownApi | (string & {});

// === Provider IDs ===
type KnownProvider = "openai" | "anthropic" | "google" | "mistral" | "xai" | "groq" | "cerebras" | "deepseek" | "openrouter" | "nvidia" | "vercel-ai-gateway" | "zai" | "minimax" | "moonshotai" | "huggingface" | "fireworks" | "together" | "opencode" | "opencode-go" | "kimi-coding" | "cloudflare-workers-ai" | "cloudflare-ai-gateway" | "xiaomi" | "github-copilot" | "ant-ling" | "amazon-bedrock" | "google-vertex" | "azure-openai-responses" | "openai-codex" | ...;
type ProviderId = KnownProvider | string;
```

### Unified Event Protocol

Every stream emits events of type `AssistantMessageEvent`:

```typescript
type AssistantMessageEvent =
  | { type: "start";            partial: AssistantMessage }
  | { type: "text_start";       contentIndex: number; partial: AssistantMessage }
  | { type: "text_delta";       contentIndex: number; delta: string;          partial: AssistantMessage }
  | { type: "text_end";         contentIndex: number; content: string;        partial: AssistantMessage }
  | { type: "thinking_start";   contentIndex: number; partial: AssistantMessage }
  | { type: "thinking_delta";   contentIndex: number; delta: string;          partial: AssistantMessage }
  | { type: "thinking_end";     contentIndex: number; content: string;        partial: AssistantMessage }
  | { type: "toolcall_start";   contentIndex: number; partial: AssistantMessage }
  | { type: "toolcall_delta";   contentIndex: number; delta: string;          partial: AssistantMessage }
  | { type: "toolcall_end";     contentIndex: number; toolCall: ToolCall;     partial: AssistantMessage }
  | { type: "done";             reason: "stop" | "length" | "toolUse";        message: AssistantMessage }
  | { type: "error";            reason: "aborted" | "error";                  error: AssistantMessage }
```

This is the **single contract** all API implementations must fulfill. Providers emit text, thinking, and tool call content interleaved, each tracked by `contentIndex` into the `partial.content[]` array.

> **Cross-reference:** This event protocol abstracts over the wire-level differences detailed in:
> - [OpenAI Chat Completions Streaming Events](../apidocs/compatibility/openAI-chat-completions/15-streaming-events.md) — Server-sent events with `delta` objects
> - [OpenAI Responses Streaming Events](../apidocs/openai-responses/07-streaming-events.md) — New event types (`response.output_text.delta`, `response.reasoning_text.delta`)
> - [Anthropic Messages API](../apidocs/anthropic-messages/messages.md) — `content_block_start`, `content_block_delta`, `content_block_stop` SSE events

---

## Models Collection

### `Models` Interface

```typescript
interface Models {
  getProviders(): readonly Provider[];
  getProvider(id: string): Provider | undefined;
  getModels(provider?: string): readonly Model<Api>[];
  getModel(provider: string, id: string): Model<Api> | undefined;
  refresh(provider?: string): Promise<void>;
  getAuth(model: Model<Api>): Promise<AuthResult | undefined>;

  stream<TApi extends Api>(model: Model<TApi>, context: Context, options?: ApiStreamOptions<TApi>): AssistantMessageEventStream;
  complete<TApi extends Api>(model: Model<TApi>, context: Context, options?: ApiStreamOptions<TApi>): Promise<AssistantMessage>;
  streamSimple(model: Model<Api>, context: Context, options?: SimpleStreamOptions): AssistantMessageEventStream;
  completeSimple(model: Model<Api>, context: Context, options?: SimpleStreamOptions): Promise<AssistantMessage>;
}
```

`complete()` is syntactic sugar: it calls `stream().result()`. `completeSimple()` calls `streamSimple().result()`.

### `createModels()` Implementation

```typescript
function createModels(options?: CreateModelsOptions): MutableModels {
  return new ModelsImpl(options);
}
```

The `ModelsImpl` class:

```typescript
class ModelsImpl implements MutableModels {
  private providers = new Map<string, Provider>();
  private credentials: CredentialStore;      // Default: InMemoryCredentialStore
  private authContext: AuthContext;           // Default: process.env + fs

  // Provider management
  setProvider(provider: Provider): void       // Upsert by id
  deleteProvider(id: string): void
  clearProviders(): void

  // Model lookup
  getModels(provider?: string): readonly Model<Api>[]
    // Best-effort: providers whose getModels() throws yield no models

  getModel(provider: string, id: string): Model<Api> | undefined
    // Calls getModels(provider).find(model => model.id === id)

  // Dynamic refresh
  async refresh(provider?: string): Promise<void>
    // Single provider: rejects with ModelsError("model_source") on failure
    // All providers: Promise.allSettled, never rejects

  // Auth
  async getAuth(model: Model<Api>): Promise<AuthResult | undefined>

  // Streaming
  stream<TApi>(model, context, options): AssistantMessageEventStream {
    return lazyStream(model, async () => {
      const { requestModel, requestOptions } = await this.applyAuth(model, options);
      const provider = this.requireProvider(model);
      return provider.stream(requestModel, context, requestOptions);
    });
  }
}
```

The key design insight: `stream()` returns a stream **synchronously** by wrapping async auth resolution in `lazyStream()`. The stream starts emitting events once auth resolves. If auth fails, the stream emits an `error` event.

### Auth Resolution Flow

```
models.stream(model, context, options)
    │
    ▼
lazyStream(model, async setup)
    │
    ▼
models.applyAuth(model, options)
    │
    ├── resolveProviderAuth(provider, model, credentials, authContext, overrides)
    │       │
    │       ├── 1. Check options.apiKey (explicit override)
    │       ├── 2. Check credential store (stored credential)
    │       │       ├── OAuth credential → resolveStoredOAuth
    │       │       │       ├── Token expired? → credentials.modify() under lock → oauth.refresh()
    │       │       │       └── oauth.toAuth() → headers + apiKey
    │       │       └── API key credential → resolveApiKey
    │       └── 3. Ambient resolution (env vars, AWS profiles, ADC files)
    │
    └── Merge auth results into options:
        - apiKey: options.apiKey ?? auth.apiKey
        - headers: { ...auth.headers, ...options.headers }
        - env: { ...authRes.env, ...options.env }
        - baseUrl on model if auth.baseUrl is set
```

> **Cross-reference:** The auth patterns here abstract over provider-specific auth described in:
> - [OpenAI Responses API Authentication](../apidocs/openai-responses/01-responses.md) — Bearer token via `Authorization` header
> - [Anthropic Messages API](../apidocs/anthropic-messages/messages.md) — `x-api-key` header

---

## Provider System

### Provider Interface

```typescript
interface Provider<TApi extends Api = Api> {
  readonly id: string;
  readonly name: string;
  readonly baseUrl?: string;
  readonly headers?: ProviderHeaders;
  readonly auth: ProviderAuth;              // At least apiKey; optionally oauth

  getModels(): readonly Model<TApi>[];       // Sync read of current model list
  refreshModels?(): Promise<void>;           // Dynamic refresh (shared in-flight promise)

  stream<T extends TApi>(model: Model<T>, context: Context, options?: ApiStreamOptions<T>): AssistantMessageEventStream;
  streamSimple(model: Model<TApi>, context: Context, options?: SimpleStreamOptions): AssistantMessageEventStream;
}
```

### `createProvider()`

The universal provider factory. Both built-in providers and custom user providers go through this:

```typescript
interface CreateProviderOptions<TApi extends Api = Api> {
  id: string;
  name?: string;                              // Default: id
  baseUrl?: string;
  headers?: ProviderHeaders;
  auth: ProviderAuth;                         // Required — every provider must declare auth
  models: readonly Model<TApi>[];             // Initial model list
  refreshModels?: () => Promise<readonly Model<TApi>[]>; // Dynamic refresh
  api: ProviderStreams | Partial<Record<TApi, ProviderStreams>>; // Single or per-API dispatch
}

function createProvider<TApi extends Api = Api>(input: CreateProviderOptions<TApi>): Provider<TApi>
```

**API dispatch logic:** If `api` is a single `ProviderStreams` object (has a `.stream` function), all models use it. If `api` is a record keyed by `model.api`, requests dispatch to the matching entry by `model.api` string. A model whose API has no entry produces a stream error.

**Dynamic refresh deduplication:**

```typescript
refreshModels: refreshModels
  ? () => {
      inflightRefresh ??= (async () => {
        try { models = await refreshModels(); }
        finally { inflightRefresh = undefined; }
      })();
      return inflightRefresh;
    }
  : undefined,
```

Concurrent calls to `refreshModels()` share a single in-flight promise.

### Built-in Provider Factories

Each provider has a factory function in `src/providers/<name>.ts`:

```typescript
// openai.ts
export function openaiProvider(): Provider<"openai-responses"> {
  return createProvider({
    id: "openai",
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    auth: { apiKey: envApiKeyAuth("OpenAI API key", ["OPENAI_API_KEY"]) },
    models: Object.values(OPENAI_MODELS),
    api: openAIResponsesApi(),   // Lazy-loaded Responses API
  });
}

// anthropic.ts
export function anthropicProvider(): Provider<"anthropic-messages"> {
  return createProvider({
    id: "anthropic",
    name: "Anthropic",
    baseUrl: "https://api.anthropic.com",
    auth: {
      apiKey: envApiKeyAuth("Anthropic API key", ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]),
      oauth: lazyOAuth({ name: "Anthropic (Claude Pro/Max)", load: loadAnthropicOAuth }),
    },
    models: Object.values(ANTHROPIC_MODELS),
    api: anthropicMessagesApi(),  // Lazy-loaded Messages API
  });
}
```

The `all.ts` aggregator:

```typescript
import { openaiProvider } from "./openai.ts";
import { anthropicProvider } from "./anthropic.ts";
// ... all 30+ providers ...

export function builtinModels(): Models {
  const models = createModels();
  models.setProvider(openaiProvider());
  models.setProvider(anthropicProvider());
  // ...
  return models;
}
```

---

## API Implementations

### The `ProviderStreams` Contract

Every API implementation module under `src/api/` exports two functions:

```typescript
interface ProviderStreams {
  stream(model: Model<Api>, context: Context, options?: StreamOptions): AssistantMessageEventStream;
  streamSimple(model: Model<Api>, context: Context, options?: SimpleStreamOptions): AssistantMessageEventStream;
}
```

The module itself is a value that satisfies this interface. Nine API implementations exist:

| Module | Wire Protocol | Used By |
|--------|--------------|---------|
| `openai-completions.ts` | `POST /v1/chat/completions` (SSE) | openai, xai, groq, cerebras, deepseek, openrouter, nvidia, together, fireworks, zai, moonshotai, minimax, cloudflare, huggingface, opencode, kimi-coding, xiaomi, ant-ling |
| `openai-responses.ts` | `POST /responses` (SSE) | openai, opencode, github-copilot |
| `openai-codex-responses.ts` | `POST /responses` (SSE + WebSocket) | openai-codex |
| `azure-openai-responses.ts` | Azure Responses API | azure-openai-responses |
| `anthropic-messages.ts` | `POST /v1/messages` (SSE) | anthropic, openrouter, fireworks, kimi-coding, github-copilot, cloudflare |
| `google-generative-ai.ts` | `generateContent` (Google AI SDK) | google |
| `google-vertex.ts` | Vertex AI SDK | google-vertex |
| `mistral-conversations.ts` | Mistral SDK | mistral |
| `bedrock-converse-stream.ts` | AWS Bedrock converseStream | amazon-bedrock |

> **Cross-reference:** Each API implementation maps to external API documentation:
> - **OpenAI Completions** → [OpenAI Chat Completions API Reference](../apidocs/compatibility/openAI-chat-completions/)
> - **OpenAI Responses** → [OpenAI Responses API Reference](../apidocs/openai-responses/)
> - **Anthropic Messages** → [Anthropic Messages API Reference](../apidocs/anthropic-messages/messages.md)

### Lazy Loading Architecture

API implementations are never imported directly by provider factories. Instead, they use `lazyApi()`:

```typescript
// openai-responses.lazy.ts
import type { ProviderStreams } from "../types.ts";
import { lazyApi } from "./lazy.ts";

export const openAIResponsesApi = (): ProviderStreams =>
  lazyApi(() => import("./openai-responses.ts"));
```

`lazyApi()` wraps the dynamic import as `ProviderStreams`:

```typescript
export function lazyApi(load: () => Promise<ProviderStreams>): ProviderStreams {
  return {
    stream: (model, context, options) =>
      lazyStream(model, async () => (await load()).stream(model, context, options)),
    streamSimple: (model, context, options) =>
      lazyStream(model, async () => (await load()).streamSimple(model, context, options)),
  };
}
```

The `import()` is only triggered on first use. The bundler keeps heavy SDK code (OpenAI SDK, Anthropic SDK, Google AI SDK) out of the initial bundle. Load failures terminate the stream with an error event — they never throw synchronously.

### OpenAI Completions API

The `openai-completions.ts` module is the most heavily used implementation, serving ~20 providers through the OpenAI-compatible chat completions protocol.

**Key flow:**

```
stream(model, context, options)
  │
  ├── Create AssistantMessageEventStream
  ├── Resolve API key (from options, headers, or provider)
  ├── Build OpenAI client (new OpenAI({ apiKey, baseURL, defaultHeaders }))
  ├── Build params (buildParams):
  │     ├── Convert messages → ChatCompletionMessageParam[]
  │     ├── Convert tools → ChatCompletionTool[]
  │     ├── Apply reasoning_effort, temperature, max_tokens
  │     ├── Apply cache control (prompt_cache_key, prompt_cache_retention)
  │     └── Apply compat settings (maxTokensField, thinkingFormat, etc.)
  ├── onPayload hook (user can inspect/modify params)
  ├── client.chat.completions.create(params).withResponse()
  ├── onResponse hook (user can inspect HTTP response)
  ├── Emit "start" event
  ├── Stream loop over SSE chunks:
  │     ├── chunk.choices[0].delta.content → text_delta events
  │     ├── chunk.choices[0].delta.reasoning_content → thinking_delta events
  │     ├── chunk.choices[0].delta.tool_calls → toolcall_delta events
  │     ├── chunk.choices[0].finish_reason → finish content blocks
  │     └── chunk.usage → populate usage counters
  ├── Emit "done" or "error" event
  └── Return
```

**Content block tracking:** During streaming, `textBlock`, `thinkingBlock`, and `toolCallBlocksByIndex` track in-progress content. When a content block finishes (via `finish_reason` or new content type starting), the corresponding `_end` event fires with the complete content.

**Error handling:** If the stream throws, `stopReason` is set to `"aborted"` (if `signal.aborted`) or `"error"`. The error message is formatted via `normalizeProviderError()` + `formatProviderError()`.

**OpenAI-specific error normalization:** The OpenAI SDK wraps errors in `APIError` objects. The utility `normalizeProviderError()` unwraps these into a standard shape before `formatProviderError()` produces the final user-facing message.

```typescript
// OpenAI error unwrapping (from error-body.ts)
function normalizeProviderError(error: unknown): NormalizedError {
  if (error instanceof OpenAI.APIError) {
    return {
      status: error.status,
      message: error.message,
      code: error.code,
      type: error.type,
    };
  }
  // Anthropic, Google, Mistral each have their own unwrapping
}
```

### OpenAI Responses API

The `openai-responses.ts` module targets the newer Responses API (`POST /responses`).

**Key differences from Completions API:**

| Aspect | Completions API | Responses API |
|--------|----------------|---------------|
| Input format | `messages[]` with role/content | `input[]` with typed items (`input_text`, `input_image`, `message`, `function_call`, etc.) |
| Output items | Inline in `choices[0].message` | Typed items in `response.output[]` |
| Reasoning | `reasoning_content` in delta | `reasoning_text` delta events |
| Tool calls | `tool_calls[]` in message | `function_call` items with `call_id` |
| Conversation | Stateless (client manages history) | Optional `conversation` parameter for server-side storage |
| Caching | `prompt_cache_key` + `prompt_cache_retention` | Same, plus `24h` retention option |
| Message identity | No IDs | Every message has an `id`; used for cross-model replay |

**Message conversion** (`convertResponsesMessages`):

1. System prompt → `{ role: "developer" | "system", content }` item (uses `developer` role when model supports reasoning)
2. User messages → `{ role: "user", content: [input_text / input_image blocks] }`
3. Assistant messages → `{ type: "message", role: "assistant", content: [{ type: "output_text", ... }], id, phase }` items
4. Tool calls → `{ type: "function_call", call_id, name, arguments }` items
5. Tool results → `{ type: "function_call_output", call_id, output }` items

**Tool call ID normalization:** OpenAI Responses API generates long IDs like `call_abc123|item_xyz789` (450+ chars). PI normalizes these to `^[a-zA-Z0-9_-]+$` (max 64 chars) for cross-provider compatibility using `shortHash()`:

```typescript
const normalizeToolCallId = (id: string, targetModel, source) => {
  if (!id.includes("|")) return normalizeIdPart(id);
  const [callId, itemId] = id.split("|");
  const normalizedCallId = normalizeIdPart(callId);
  const isForeign = source.provider !== model.provider || source.api !== model.api;
  let normalizedItemId = isForeign
    ? `fc_${shortHash(itemId)}`       // Foreign → hash
    : normalizeIdPart(itemId);         // Same provider → clean
  return `${normalizedCallId}|${normalizedItemId}`;
};
```

**Stream processing** (`processResponsesStream`): Handles the Responses API event types:
- `response.output_text.delta` → `text_delta`
- `response.reasoning_text.delta` → `thinking_delta`
- `response.function_call_arguments.delta` → `toolcall_delta`
- `response.output_item.done` → finish content blocks
- `response.completed` → populate usage, finalize

> **Cross-reference:** The Responses API event types are fully detailed in:
> - [OpenAI Responses Streaming Events](../apidocs/openai-responses/07-streaming-events.md)
> - [OpenAI Responses Create](../apidocs/openai-responses/02-create.md) — Full input/output item schema

### Anthropic Messages API

The `anthropic-messages.ts` module targets `POST /v1/messages`.

**Stealth mode:** When the provider has a `claude-code` compat flag, PI mimics Claude Code's tool naming (canonical casing: `Read`, `Write`, `Edit`, `Bash`, `Grep`, etc.):

```typescript
const claudeCodeTools = ["Read", "Write", "Edit", "Bash", "Grep", "Glob", ...];
const toClaudeCodeName = (name: string) => ccToolLookup.get(name.toLowerCase()) ?? name;
const fromClaudeCodeName = (name: string, tools?: Tool[]) => { /* reverse lookup */ };
```

**Message conversion differences from OpenAI:**

- **Content blocks:** Anthropic uses typed content blocks (`text`, `image`, `tool_use`, `tool_result`) natively — similar to the unified types but with different field names
- **System prompt:** Separate top-level `system` parameter, not a message role
- **Tools:** `tools[]` with `name`, `description`, `input_schema`
- **Cache control:** `cache_control: { type: "ephemeral" }` markers on content blocks (Anthropic's native mechanism) rather than a separate `prompt_cache_key` parameter
- **Thinking:** `thinking: { type: "enabled", budget_tokens }` or `thinking: { type: "adaptive" }` parameter

**Prompt caching:** PI maps `cacheRetention: "long"` → Anthropic's `ttl: "1h"` cache control (vs default 5m) when the model supports it.

> **Cross-reference:** Full Anthropic Messages API schema in:
> - [Anthropic Messages API](../apidocs/anthropic-messages/messages.md) — Complete param reference for `POST /v1/messages`

---

## Stream & Event System

### `EventStream<T, R>`

A generic async-iterable event stream class in `src/utils/event-stream.ts`:

```typescript
class EventStream<T, R = T> implements AsyncIterable<T> {
  private queue: T[] = [];
  private waiting: ((value: IteratorResult<T>) => void)[] = [];
  private done = false;
  private finalResultPromise: Promise<R>;

  push(event: T): void    // Deliver to consumer or queue
  end(result?: R): void   // Resolve final promise, notify all waiters

  async *[Symbol.asyncIterator](): AsyncIterator<T>  // Pull from queue or await
  result(): Promise<R>    // Returns final result after stream completes
}
```

**Key design decisions:**

1. **Push-based with pull interface:** Events are pushed into the stream by the async producer, but consumed via `for await...of` by synchronous-looking consumer code.
2. **Backpressure-free:** The queue grows unboundedly. Designed for streaming LLM responses where the producer naturally generates events at ~50 tokens/sec.
3. **Dual termination:** The stream terminates via a "terminal event" (checked by `isComplete(event)`). The `result()` promise resolves with the extracted final value.
4. **No buffering lost:** If the consumer hasn't started iterating when events arrive, they queue up in `this.queue`.

### `AssistantMessageEventStream`

A typed specialization:

```typescript
class AssistantMessageEventStream extends EventStream<AssistantMessageEvent, AssistantMessage> {
  constructor() {
    super(
      (event) => event.type === "done" || event.type === "error",  // isComplete
      (event) => {                                                  // extractResult
        if (event.type === "done") return event.message;
        if (event.type === "error") return event.error;
        throw new Error("Unexpected event type");
      },
    );
  }
}
```

The stream completes on `done` or `error` events. The final result is always an `AssistantMessage` — either the successful message from `done` or the error-bearing message from `error`.

### Event Lifecycle

```
start ──► text_start ──► text_delta* ──► text_end
     │
     ├──► thinking_start ──► thinking_delta* ──► thinking_end
     │
     ├──► toolcall_start ──► toolcall_delta* ──► toolcall_end
     │
     └──► [more content blocks ...]
              │
              ▼
            done ──► stream.result() resolves
         or error ──► stream.result() resolves (with error message)
```

`text_delta`, `thinking_delta`, and `toolcall_delta` can fire many times. `text_end`/`thinking_end`/`toolcall_end` carry the finalized content. All content blocks share a single `partial: AssistantMessage` reference, allowing consumers to inspect partial state at any point.

### Lazy Streams (`lazyStream`)

```typescript
export function lazyStream(
  model: Model<Api>,
  setup: () => Promise<AsyncIterable<AssistantMessageEvent>>,
): AssistantMessageEventStream {
  const outer = new AssistantMessageEventStream();

  setup()
    .then((inner) => { forwardStream(outer, inner); })
    .catch((error) => {
      const message = createSetupErrorMessage(model, error);
      outer.push({ type: "error", reason: "error", error: message });
      outer.end(message);
    });

  return outer;
}
```

Returns a stream synchronously. Async setup (auth, lazy loading) runs in the background. Its events are forwarded to the outer stream. Setup failures become `error` events.

The `forwardStream()` helper just iterates the inner async iterable and pushes each event to the outer stream:

```typescript
function forwardStream(target, source) {
  (async () => {
    for await (const event of source) { target.push(event); }
    target.end();
  })();
}
```

---

## Auth Subsystem

### Auth Types

```typescript
interface ProviderAuth {
  apiKey: ApiKeyAuth;           // Required — every provider must have API key auth
  oauth?: OAuthAuth;             // Optional — OAuth for subscription-based access
}

interface ApiKeyAuth {
  name: string;
  resolve: (input: { model: AuthModel; ctx: AuthContext; credential?: ApiKeyCredential })
    => Promise<AuthResult | undefined>;
  login: (callbacks: LoginCallbacks) => Promise<ApiKeyCredential>;
}

interface OAuthAuth {
  name: string;
  login: (callbacks: OAuthLoginCallbacks) => Promise<OAuthCredential>;
  refresh: (credential: OAuthCredential) => Promise<OAuthCredential>;
  toAuth: (credential: OAuthCredential) => Promise<{ apiKey: string; headers?: Record<string, string>; baseUrl?: string }>;
}

interface AuthResult {
  auth: { apiKey: string; headers?: Record<string, string>; baseUrl?: string };
  source: string;    // e.g., "OPENAI_API_KEY", "stored credential", "OAuth"
}
```

### Credential Store

```typescript
interface CredentialStore {
  read(providerId: string): Promise<Credential | undefined>;
  write(providerId: string, credential: Credential): Promise<void>;
  delete(providerId: string): Promise<void>;
  modify(providerId: string, fn: (current: Credential | undefined) => Promise<Credential | undefined>): Promise<Credential | undefined>;
  close?(): void;
}
```

The default is `InMemoryCredentialStore` — a simple `Map<string, Credential>`. The `modify()` method provides double-checked locking for OAuth token refresh (see below).

### API Key Auth

The standard factory is `envApiKeyAuth()`:

```typescript
function envApiKeyAuth(name: string, envVars: readonly string[]): ApiKeyAuth {
  return {
    name,
    login: async (callbacks) => {
      const key = await callbacks.prompt({ type: "secret", message: `Enter ${name}` });
      return { type: "api_key", key };
    },
    resolve: async ({ ctx, credential }) => {
      if (credential?.key) return { auth: { apiKey: credential.key }, source: "stored credential" };
      for (const envVar of envVars) {
        const value = await ctx.env(envVar);
        if (value) return { auth: { apiKey: value }, source: envVar };
      }
      return undefined;  // Unconfigured
    },
  };
}
```

Resolution priority: stored credential → first set environment variable → `undefined` (unconfigured).

Custom providers with non-standard auth (AWS IAM, ADC files) write their own `ApiKeyAuth` with a custom `resolve()`.

### OAuth Auth

For subscription-based providers (OpenAI Codex, GitHub Copilot, Anthropic Pro/Max). The `resolveStoredOAuth()` function implements double-checked locking:

```typescript
async function resolveStoredOAuth(credentials, providerId, oauth, stored) {
  let credential = stored;

  if (Date.now() >= credential.expires) {
    // Optimistic check said expired → lock and re-check
    let post = await credentials.modify(providerId, async (current) => {
      if (current?.type !== "oauth") return undefined;       // Logged out
      if (Date.now() < current.expires) return undefined;    // Another process refreshed
      return await oauth.refresh(current);                   // We refresh
    });
    credential = post;   // Updated credential or undefined
  }

  return { auth: await oauth.toAuth(credential), source: "OAuth" };
}
```

### Lazy OAuth

```typescript
function lazyOAuth(input: { name: string; load: () => Promise<OAuthAuth> }): OAuthAuth {
  let promise: Promise<OAuthAuth> | undefined;
  return {
    name: input.name,
    login: async (callbacks) => (await (promise ??= input.load())).login(callbacks),
    refresh: async (credential) => (await (promise ??= input.load())).refresh(credential),
    toAuth: async (credential) => (await (promise ??= input.load())).toAuth(credential),
  };
}
```

Keeps Node-only OAuth flow code out of bundles. The `load()` function uses a bundler-opaque dynamic import (variable specifier), so the heavy OAuth implementation is tree-shaken until first use.

### Resolution Pipeline

```
resolveProviderAuth(provider, model, credentials, authContext, overrides?)
    │
    ├── 1. Explicit apiKey override? → resolveApiKey with the override
    │
    ├── 2. Stored credential exists?
    │       ├── OAuth credential + provider has oauth handler → resolveStoredOAuth
    │       └── API key credential + provider has apiKey handler → resolveApiKey
    │
    └── 3. Ambient: provider.auth.apiKey.resolve(ctx, credential=undefined)
            → returns undefined if no env var set (unconfigured)
```

Auth failures produce `ModelsError` with codes:
- `"oauth"` — Token refresh or auth derivation failed (credential preserved for retry)
- `"auth"` — API key resolution or credential store operation failed

---

## Context & Message Model

### Unified Message Types

```typescript
type Message = UserMessage | AssistantMessage | ToolResultMessage;

interface Context {
  systemPrompt?: string;
  messages: Message[];
  tools?: Tool[];
}
```

The `Context` is fully serializable — it contains no functions, only data. This enables:
- Saving/restoring conversation state to disk
- Transferring conversations between providers (cross-provider handoff)
- Sending context over the network

### Content Blocks

Assistant messages carry an array of heterogeneous content blocks:

```typescript
type AssistantContent = (TextContent | ThinkingContent | ToolCall)[];
```

This is a flat list — content blocks are ordered as they streamed in. A single assistant message might contain: `[thinking, text, toolCall, text, toolCall]` — reflecting the model's interleaved reasoning, output, and tool use.

### Context Serialization

The `Context` type is plain JSON-serializable. PI does not prescribe a serialization format — consumers are free to use `JSON.stringify`/`JSON.parse`. The library provides no built-in serialization, relying on the type system to keep context portable.

This is distinct from OpenAI's server-side `conversation` parameter (Responses API) which stores history server-side. PI is purely client-side — the application owns conversation state.

> **Cross-reference:** OpenAI's server-side conversation state is documented in:
> - [Conversation State](../apidocs/compatibility/openAI-chat-completions/07-conversation-state.md) — Chat Completions `store: true`
> - [OpenAI Responses Create](../apidocs/openai-responses/02-create.md) — `conversation` parameter

---

## Cross-Provider Message Transformation

### `transformMessages()`

When switching models mid-session (cross-provider handoff), message history must be normalized for the target model. The `transformMessages()` function in `src/api/transform-messages.ts` handles this.

```typescript
function transformMessages<TApi extends Api>(
  messages: Message[],
  model: Model<TApi>,
  normalizeToolCallId?: (id: string, model: Model<TApi>, source: AssistantMessage) => string,
): Message[]
```

**Transformation passes:**

1. **Null content normalization** — `null`/`undefined` content → empty array
2. **Image downgrading** — Replace images with placeholders for non-vision models
3. **Message transformation** — Thinking blocks, tool call IDs, text signatures
4. **Orphaned tool call insertion** — Synthetic tool results for unmatched tool calls

### Tool Call ID Normalization

Different providers use different ID formats:

| Provider | Format | Example |
|----------|--------|---------|
| OpenAI Responses | `call_XXXX\|item_YYYY` | `call_abc123\|item_xyz789` (450+ chars) |
| Anthropic | `toolu_XXXX` | `toolu_01ABC123...` (alphanumeric + underscore + hyphen) |
| OpenAI Completions | `call_XXXX` | `call_abc123` |

The `normalizeToolCallId` callback (provided by each API implementation) normalizes IDs to match the target model's requirements. For OpenAI Responses → Anthropic handoff, the double-barreled IDs get hashed into valid `toolu_` format.

### Thinking Block Downgrading

When switching models (or same model with different settings):

- **Redacted thinking** (from safety filters) → **dropped** if target isn't the exact same model
- **Empty thinking** (OpenAI encrypted reasoning) → **dropped**
- **Non-empty thinking** → **converted to text** if target is a different model (preserving the thought content)
- **Same model** → thinking blocks pass through with signatures for replay continuity

```typescript
if (block.type === "thinking") {
  if (block.redacted) return isSameModel ? block : [];           // Drop for other models
  if (isSameModel && block.thinkingSignature) return block;       // Preserve for replay
  if (!block.thinking || block.thinking.trim() === "") return []; // Drop empty
  if (isSameModel) return block;                                  // Same model: keep
  return { type: "text", text: block.thinking };                  // Cross-model: convert to text
}
```

### Image Placeholding

Non-vision models receive placeholder text instead of images:

```typescript
function downgradeUnsupportedImages(messages, model) {
  if (model.input.includes("image")) return messages;   // Vision model: no change
  return messages.map(msg => {
    if (msg.role === "user" && Array.isArray(msg.content))
      return { ...msg, content: replaceImagesWithPlaceholder(msg.content, "(image omitted: model does not support images)") };
    if (msg.role === "toolResult")
      return { ...msg, content: replaceImagesWithPlaceholder(msg.content, "(tool image omitted: model does not support images)") };
    return msg;
  });
}
```

### Orphaned Tool Call Handling

When an assistant message contains tool calls but the subsequent tool result was lost (error, abort, or incomplete handoff), `transformMessages()` inserts synthetic error results:

```typescript
// Second pass: insert synthetic empty tool results for orphaned tool calls
if (pendingToolCalls.length > 0) {
  for (const tc of pendingToolCalls) {
    if (!existingToolResultIds.has(tc.id)) {
      result.push({
        role: "toolResult",
        toolCallId: tc.id,
        toolName: tc.name,
        content: [{ type: "text", text: "No result provided" }],
        isError: true,
        timestamp: Date.now(),
      });
    }
  }
}
```

This prevents API errors from unmatched tool calls (many APIs require every `tool_use` to have a corresponding `tool_result`).

Additionally, **errored/aborted assistant messages are skipped entirely** during replay — they represent incomplete turns and can cause API errors (e.g., OpenAI "reasoning without following item").

---

## Streaming Modes

### `stream` vs `streamSimple`

PI offers two streaming APIs:

| Method | Options Type | Purpose |
|--------|-------------|---------|
| `model.stream()` / `models.stream()` | Provider-specific (`ApiStreamOptions<TApi>`) | Full control: provider-specific params like `reasoningEffort`, `toolChoice`, `serviceTier` |
| `model.streamSimple()` / `models.streamSimple()` | `SimpleStreamOptions` | Simplified: just `temperature`, `maxTokens`, `reasoning` (ThinkingLevel), `thinkingBudgets` |

```typescript
interface SimpleStreamOptions extends StreamOptions {
  reasoning?: ThinkingLevel;           // "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
  thinkingBudgets?: ThinkingBudgets;   // Custom token budgets per level
}

interface ThinkingBudgets {
  minimal?: number;   // Default: 1024
  low?: number;       // Default: 2048
  medium?: number;    // Default: 8192
  high?: number;      // Default: 16384
}
```

### Thinking/Reasoning in `streamSimple`

`streamSimple` maps the unified `ThinkingLevel` to provider-specific reasoning parameters:

```typescript
// OpenAI Responses example:
const clampedReasoning = options?.reasoning
  ? clampThinkingLevel(model, options.reasoning)   // Respect model's thinkingLevelMap
  : undefined;
const reasoningEffort = clampedReasoning === "off" ? undefined : clampedReasoning;

// Anthropic example:
const { maxTokens, thinkingBudget } = adjustMaxTokensForThinking(
  baseMaxTokens, model.maxTokens, reasoningLevel, customBudgets
);
params.thinking = { type: "enabled", budget_tokens: thinkingBudget };
```

The `clampThinkingLevel()` function consults the model's `thinkingLevelMap` to determine which levels are actually supported.

### Token Budget Management

`adjustMaxTokensForThinking()` in `simple-options.ts`:

```typescript
function adjustMaxTokensForThinking(baseMaxTokens, modelMaxTokens, reasoningLevel, customBudgets) {
  const budgets = { ...defaultBudgets, ...customBudgets };
  let thinkingBudget = budgets[clampReasoning(reasoningLevel)];
  const maxTokens = baseMaxTokens === undefined
    ? modelMaxTokens
    : Math.min(baseMaxTokens + thinkingBudget, modelMaxTokens);

  // Ensure at least 1024 tokens for output after thinking
  if (maxTokens <= thinkingBudget) {
    thinkingBudget = Math.max(0, maxTokens - 1024);
  }

  return { maxTokens, thinkingBudget };
}
```

`"xhigh"` and `"max"` are clamped to `"high"` for providers that don't support them. The function also includes `clampMaxTokensToContext()` which checks the model's `contextWindow` and estimated context token usage:

```typescript
function clampMaxTokensToContext(model, context, maxTokens) {
  if (model.contextWindow <= 0) return Math.max(1, maxTokens);
  const available = model.contextWindow - estimateContextTokens(context).tokens - 4096;  // Safety margin
  return Math.min(maxTokens, Math.max(1, available));
}
```

---

## Cost & Usage Tracking

### Tiered Pricing

Models can define cost tiers based on cumulative input tokens:

```typescript
interface ModelCostRates {
  input: number;           // Per 1M tokens
  output: number;
  cacheRead: number;
  cacheWrite: number;
  tiers?: ModelCostRates[]; // Each tier: inputTokensAbove threshold
}
```

```typescript
function calculateCost(model, usage) {
  const inputTokens = usage.input + usage.cacheRead + usage.cacheWrite;
  let rates = model.cost;
  for (const tier of model.cost.tiers ?? []) {
    if (inputTokens > tier.inputTokensAbove && tier.inputTokensAbove > matchedThreshold) {
      rates = tier;
    }
  }
  // Apply rates to token counts
  usage.cost.input = (rates.input / 1_000_000) * usage.input;
  // ... etc
}
```

### Service Tier Multipliers

OpenAI Responses API supports `flex` and `priority` service tiers with cost multipliers:

```typescript
function getServiceTierCostMultiplier(model, serviceTier) {
  switch (serviceTier) {
    case "flex":     return 0.5;
    case "priority": return model.id === "gpt-5.5" ? 2.5 : 2;
    default:         return 1;
  }
}
```

### Prompt Caching Costs

Usage tracks cache reads and writes separately:

```typescript
interface Usage {
  input: number;
  output: number;
  cacheRead: number;    // Tokens read from cache (discounted)
  cacheWrite: number;   // Tokens written to cache (Anthropic: 1.25x base; OpenAI: no surcharge)
  cacheWrite1h?: number; // Subset of cacheWrite with 1h retention (Anthropic: 2x base input)
  cost: {
    input: number;       // (rates.input / 1M) * input
    output: number;      // (rates.output / 1M) * output
    cacheRead: number;   // (rates.cacheRead / 1M) * cacheRead
    cacheWrite: number;  // (rates.cacheWrite / 1M) * cacheWrite
    total: number;
  };
}
```

---

## Model Catalog

### Static Model Definitions

Models are defined as typed objects with capabilities and pricing. Example from `openai.models.ts`:

```typescript
export const OPENAI_MODELS = {
  "gpt-5.6": {
    id: "gpt-5.6",
    provider: "openai",
    api: "openai-responses",
    input: ["text", "image"],
    contextWindow: 272_000,
    maxTokens: 128_000,
    cost: {
      input: 1.25,        // Per 1M tokens
      output: 10.0,
      cacheRead: 0.625,
      cacheWrite: 1.25,
      tiers: [{ inputTokensAbove: 200_000, input: 2.5, output: 20.0, cacheRead: 1.25, cacheWrite: 2.5 }],
    },
    reasoning: true,
    thinkingLevelMap: {
      minimal: "minimal",
      low: "low",
      medium: "medium",
      high: "high",
    },
  },
  // ...
};
```

Key model properties:

| Property | Type | Description |
|----------|------|-------------|
| `id` | `string` | Model identifier (e.g., `"gpt-5.6"`) |
| `provider` | `ProviderId` | Owning provider |
| `api` | `Api` | Wire protocol to use |
| `input` | `("text" \| "image" \| "audio")[]` | Supported input modalities |
| `contextWindow` | `number` | Max context tokens (0 = unknown) |
| `maxTokens` | `number` | Max output tokens |
| `cost` | `ModelCostRates` | Per-1M-token pricing with optional tiers |
| `reasoning` | `boolean` | Whether the model supports reasoning/thinking |
| `thinkingLevelMap` | `ThinkingLevelMap` | Maps `ThinkingLevel` to provider-specific effort strings or `null` (unsupported) |
| `compat` | `Record<string, unknown>` | Provider-specific compatibility flags |
| `headers` | `ProviderHeaders` | Additional headers for this model |

### Dynamic Providers (`refreshModels`)

Providers that fetch their model list from an API (OpenRouter, Vercel AI Gateway, Cloudflare AI Gateway) implement `refreshModels`. The `models.json` manifest format is also supported for custom model lists.

### Model Capabilities (`input`)

The `input` array declares supported modalities:
- `"text"` — Text input (all models)
- `"image"` — Vision/image input
- `"audio"` — Audio input (not currently used by any PI model — see [Capability Gap Analysis](#capability-gap-analysis-what-pi-lacks))

### Thinking Level Maps

```typescript
type ThinkingLevel = "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
type ModelThinkingLevel = "off" | ThinkingLevel;
type ThinkingLevelMap = Partial<Record<ModelThinkingLevel, string | null>>;
```

A value of `null` means the level is unsupported. `"off"` → `null` means reasoning can't be disabled for that model. Provider implementations consult this map to translate unified levels to provider-specific values (e.g., OpenAI's `reasoning_effort`, Anthropic's `budget_tokens`).

---

## Image Generation

PI includes a separate `ImagesModels` collection with its own `ImagesProvider`, `ImagesModel`, and `ImagesContext` types. Currently only OpenRouter is supported for image generation via the `openrouter-images.ts` API implementation.

```typescript
interface ImagesContext {
  input: ImagesInputContent[];   // TextContent | ImageContent (reference images)
}

interface AssistantImages {
  api: ImagesApi;
  provider: ImagesProviderId;
  model: string;
  output: ImagesOutputContent[];  // TextContent | ImageContent (generated images)
  usage?: Usage;
  stopReason: ImagesStopReason;
}
```

The image generation flow is simpler than chat — it's a single `generateImages()` call returning a `Promise<AssistantImages>`, not a stream.

---

## Custom Providers & OpenAI Compatibility

### `createProvider()` for Custom Endpoints

Any OpenAI-compatible API (Ollama, vLLM, LM Studio, local endpoints) can be added:

```typescript
import { createProvider, createModels } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions";

const models = createModels();
models.setProvider(createProvider({
  id: "my-ollama",
  name: "Ollama (Local)",
  baseUrl: "http://localhost:11434/v1",
  auth: {
    apiKey: {
      name: "Ollama",
      resolve: async () => ({ auth: { apiKey: "ollama" }, source: "local" }),
      login: async (c) => ({ type: "api_key", key: "ollama" }),
    },
  },
  models: [{
    id: "llama3.1:8b",
    provider: "my-ollama",
    api: "openai-completions",
    input: ["text"],
    contextWindow: 131072,
    maxTokens: 8192,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  }],
  api: openAICompletionsApi(),
}));
```

### OpenAI Compatibility Settings

For providers that deviate from strict OpenAI compatibility, the `OpenAICompletionsCompat` interface on the model's `compat` field controls behavior:

```typescript
interface OpenAICompletionsCompat {
  supportsStore?: boolean;                        // store: true for conversation persistence
  supportsDeveloperRole?: boolean;                // "developer" vs "system" role
  supportsReasoningEffort?: boolean;              // reasoning_effort parameter
  supportsUsageInStreaming?: boolean;             // stream_options: { include_usage: true }
  maxTokensField?: "max_completion_tokens" | "max_tokens";  // Which field for max tokens
  requiresToolResultName?: boolean;               // name field on tool results
  requiresAssistantAfterToolResult?: boolean;     // Assistant message between tool result and user
  requiresThinkingAsText?: boolean;               // Convert thinking to <thinking> text blocks
  requiresReasoningContentOnAssistantMessages?: boolean; // Empty reasoning_content on replayed assistant
  thinkingFormat?: "openai" | "openrouter" | "deepseek" | "together" | "zai" | "qwen" | "qwen-chat-template" | "chat-template" | "string-thinking" | "ant-ling";
  cacheControlFormat?: "openai" | "anthropic" | "qwen";    // Prompt caching format
}
```

These compat flags are auto-detected from the provider's `baseUrl` for known providers but can be explicitly set for custom endpoints.

---

## Capability Gap Analysis: What PI Lacks

PI's design is intentionally scoped to **tool-calling-capable text models**. Several OpenAI API capabilities documented in the `apidocs/` directory are out of scope or not yet implemented.

### Audio Input/Output

**Status: Not implemented.** PI's unified types have no `AudioContent` block and no `"audio"` input modality.

**What OpenAI supports** (see [Audio and Speech](../apidocs/compatibility/openAI-chat-completions/04-audio-speech.md)):

| Capability | API | Models |
|-----------|-----|--------|
| Audio input | Chat Completions with `input_audio` content blocks | `gpt-audio-1.5` |
| Audio output | Chat Completions with `modalities: ["text", "audio"]` + `audio: { voice, format }` | `gpt-audio-1.5` |
| Realtime speech-to-speech | WebRTC sessions via `RealtimeAgent` / `RealtimeSession` | `gpt-realtime-2.1` |
| Speech-to-text (transcription) | `POST /v1/audio/transcriptions` | `whisper-1` |
| Text-to-speech | `POST /v1/audio/speech` | `tts-1`, `tts-1-hd` |

**What PI would need to add:**

1. An `AudioContent` block type: `{ type: "audio"; data: string; format: "wav" | "mp3"; }`
2. An `"audio"` entry in the `input` model capability array
3. Audio message conversion in each API implementation
4. A `RealtimeSession` abstraction for WebRTC-based providers (currently completely absent)
5. Separate API endpoints for transcription/TTS (not part of the unified chat stream)

**Gap significance:** Audio is the fastest-growing modality in AI. Voice agents and realtime interactions are a major use case that PI cannot serve. However, PI's focus on agentic/tool-calling models means audio may be intentionally deferred to keep the API surface manageable.

### Realtime Sessions

**Status: Not implemented.** PI has no concept of persistent realtime connections. The `transport` option (`"sse" | "websocket" | "websocket-cached" | "auto"`) exists but is only used by `openai-codex-responses.ts`.

OpenAI's realtime API uses WebRTC for bidirectional audio streaming with persistent session state. PI's architecture (request → stream → result) is fundamentally request-response and would need significant rework for realtime.

### Structured Outputs (`json_schema`)

**Status: Not implemented.** PI's `Tool` type uses TypeBox schemas for tool parameter validation but does not pass `response_format: { type: "json_schema", json_schema: {...} }` to the API.

**What OpenAI supports** (see [Structured Outputs](../apidocs/compatibility/openAI-chat-completions/05-structured-outputs.md)):
- `text.format: { type: "json_schema", ... }` on Responses API
- `response_format: { type: "json_schema", ... }` on Chat Completions API
- Guaranteed schema adherence with programmatic refusal detection

**What PI would need:** A `responseFormat` option in `StreamOptions` and conversion logic in each API implementation. The TypeBox schemas already exist on tools.

### Conversation State (Store/Retrieve)

**Status: Partially absent.** PI manages conversation state entirely client-side via the `Context.messages[]` array. It does not use OpenAI's server-side conversation storage.

**What OpenAI supports** (see [Conversation State](../apidocs/compatibility/openAI-chat-completions/07-conversation-state.md)):
- `store: true` on Chat Completions → server stores the completion
- `conversation: { id }` on Responses API → server appends to conversation
- Retrieve/list/update/delete stored completions
- List messages in a stored completion

**What PI has:** The `supportsStore` compat flag on OpenAI Completions models, but it's used only for `store: false` (opt-out) — never `store: true` for server-side storage.

### Web Search / File Search / Code Interpreter Tools

**Status: Not implemented.** These are "server tools" — executed by the API provider, not the client.

**What OpenAI supports** (see [Responses API Create](../apidocs/openai-responses/02-create.md)):
- `tools: [{ type: "web_search" }]` — Built-in web search
- `tools: [{ type: "file_search", vector_store_ids: [...] }]` — Semantic file search
- `tools: [{ type: "code_interpreter" }]` — Python code execution in sandbox

**What PI would need:** New tool types in the unified type system and conversion logic that emits the correct `tools[]` entries for each API. The response handling would need to understand `web_search_call`, `file_search_call`, and `code_interpreter_call` output items.

### Computer Use / Browser Use

**Status: Not implemented.** PI has no computer-use primitives.

**What OpenAI supports** (see [Responses API Create](../apidocs/openai-responses/02-create.md)):
- `tools: [{ type: "computer_use_preview", display_width, display_height, environment }]`
- Actions: click, double_click, drag, keypress, move, screenshot, scroll, type, wait
- Safety check acknowledgement

**What Anthropic supports** (see [Anthropic Messages API](../apidocs/anthropic-messages/messages.md)):
- `tools: [{ type: "computer_20250124", ... }]` (Anthropic's computer use tool)
- `tools: [{ type: "bash_20250124" }]` (Anthropic's bash tool)
- `tools: [{ type: "text_editor_20250124" }]` (Anthropic's text editor tool)
- `tools: [{ type: "web_search_20250305" }]` (Anthropic's web search tool)
- `tools: [{ type: "memory_20250818" }]` (Anthropic's memory tool)

These are all supported by Anthropic's Messages API but not exposed through PI's unified interface. Terminus-2 (the other refImpl in this directory) handles computer interaction through a completely different mechanism — raw tmux keystrokes rather than structured computer-use tools.

### Background Responses

**Status: Not implemented.** PI's `stream()`/`complete()` are synchronous request-response. OpenAI's `background: true` pattern for long-running operations is not supported.

### Mid-Conversation System Messages

**Status: Not implemented.** PI only supports a single `systemPrompt` at the top of `Context`. Anthropic's `mid_conv_system` content blocks for updating system instructions mid-conversation are not exposed.

---

## Cross-References to API Documentation

| PI Component | API Documentation |
|-------------|-------------------|
| `openai-completions.ts` stream/streamSimple | [OpenAI Chat Completions](../apidocs/compatibility/openAI-chat-completions/) — Full API reference (15 docs) |
| `openai-responses.ts` stream/streamSimple | [OpenAI Responses API](../apidocs/openai-responses/) — Full API reference (10 docs) |
| `anthropic-messages.ts` stream/streamSimple | [Anthropic Messages API](../apidocs/anthropic-messages/messages.md) — Complete param reference |
| `transformMessages()` message normalization | [Conversation State](../apidocs/compatibility/openAI-chat-completions/07-conversation-state.md) — Cross-model message replay |
| `Message` / `Context` types | [OpenAI Responses Input Items](../apidocs/openai-responses/09-input-items-list.md) — Input item types |
| `Usage` / cost tracking | [OpenAI Responses Input Tokens](../apidocs/openai-responses/10-input-tokens-count.md) — Token counting API |
| `Tool` / `ToolCall` types | [Function Calling](../apidocs/compatibility/openAI-chat-completions/06-function-calling.md) — Tool definitions and handling |
| Event protocol (text_start, text_delta, etc.) | [Streaming Events (Completions)](../apidocs/compatibility/openAI-chat-completions/15-streaming-events.md) and [Streaming Events (Responses)](../apidocs/openai-responses/07-streaming-events.md) |
| `reasoningEffort` / `thinking` params | [Structured Outputs](../apidocs/compatibility/openAI-chat-completions/05-structured-outputs.md) — Reasoning configuration |
| Image content blocks | [Images & Vision](../apidocs/compatibility/openAI-chat-completions/03-images-vision.md) — Image input types |
| *(Gap)* Audio input/output | [Audio and Speech](../apidocs/compatibility/openAI-chat-completions/04-audio-speech.md) — Full audio modalities |
| *(Gap)* WebSocket transport | [WebSocket Events (Responses)](../apidocs/openai-responses/08-websocket-events.md) — Realtime WebSocket protocol |
| *(Gap)* Response compaction | [Compact Response](../apidocs/openai-responses/05-compact.md) — Context management |
| *(Gap)* Background responses | [Responses API Overview](../apidocs/openai-responses/01-responses.md) — `background: true` |
| *(Gap)* Response retrieve/cancel/delete/list | [Retrieve](../apidocs/openai-responses/03-retrieve.md), [Cancel](../apidocs/openai-responses/04-cancel.md), [Delete](../apidocs/openai-responses/06-delete.md) — Response lifecycle |
| *(Gap)* Chat completion CRUD | [Retrieve](../apidocs/compatibility/openAI-chat-completions/10-retrieve-chat-completion.md) through [List](../apidocs/compatibility/openAI-chat-completions/12-delete-chat-completion.md) — Server-side storage |

---

## Summary of Design Decisions

| Decision | Rationale |
|----------|-----------|
| Single `AssistantMessageEventStream` for all providers | Consumers write one event handler regardless of backend |
| Lazy-loaded API implementations via `lazyApi()` | Tree-shaking keeps unused providers out of bundles |
| `lazyStream()` returning synchronously with async auth | Callers never block on auth; failures surface as stream events |
| Auth as declarative data on `Provider` | Model → Provider → Auth is a pure lookup chain |
| `streamSimple` with `SimpleStreamOptions` | 90% of use cases need only temperature + reasoning level |
| Double-checked locking for OAuth token refresh | Prevents thundering-herd token refresh under concurrent requests |
| `transformMessages()` with image placeholding + ID normalization | Enables cross-provider handoff without API errors |
| Tiered pricing with service-tier multipliers | Accurate cost tracking for OpenAI flex/priority tiers |
| Only tool-calling-capable models | Library is for agentic workflows; embedding/audio models are excluded |
| Unified `ThinkingLevel` (6 levels) mapped per-model | Single API for reasoning across OpenAI, Anthropic, Google, DeepSeek |
