# Ygg Extension Protocol Reference

> **API versions:** `0.1` (frozen compatibility) and `0.2` (current,
> `EXTENSION_API_VERSION`)
>
> Every request and response uses the standard JSON-RPC 2.0 envelope with
> exactly one JSON object per line on **stdout**. Human diagnostics belong on
> **stderr**, which Ygg drains and exposes as bounded diagnostic events.
>
> Extensions send process-to-host messages at any time after initialization.
> For graceful shutdown, the host sends a JSON-RPC `shutdown` request; the
> extension acknowledges it and exits promptly. Stdin EOF means the transport
> was lost or finally torn down and should also make the extension exit.

The manifest selects exactly one wire version. API `0.1` remains available,
byte-compatible at initialization, for trusted text-oriented extensions. It
does not gain API `0.2` cancellation, progress, structured/media retention,
parent-correlation, or terminal lifecycle guarantees. API `0.2` adds those
stateful guarantees through explicit initialization negotiation; support is
never inferred from the extension package version.

This protocol is the bus of a deliberately small agent kernel. Ygg hosts model
conversations, session/result persistence, permissions and approvals, process
supervision/cleanup, and resource limits. MCP, browser use, computer use, web
search, memory, LSP, subagent orchestration, and caffeinate remain replaceable
subprocess extensions. Generic host services in this document support those
extensions without moving their domain protocols into the host.

Implemented limits in this reference are protocol, queue, concurrency,
artifact, timeout, and process-tree cleanup bounds. Ygg does not yet enforce OS
CPU/RSS/FD/PID quotas or sandbox trusted extensions; they run with the current
user's authority.

## Transport defaults

| Parameter | Default |
|---|---|
| Max JSON line | 1 MiB (`DEFAULT_EXTENSION_MESSAGE_BYTES`) |
| Host in-flight requests | 64 (`DEFAULT_PENDING_REQUESTS`); API `0.2` may negotiate lower |
| Complete frames waiting for the serialized writer | 128 (`DEFAULT_WRITER_QUEUE`) |
| Request timeout | 30 s |
| API `0.2` cancellation grace | 2 s |
| Cancelled-ID tombstone retention | 30 s, at most 512 IDs |
| Normal shutdown request/ack stage | 2 s (`ExtensionRuntimeConfig::shutdown_timeout`) |
| Normal post-request process-exit stage | 2 s (the same per-stage timeout) |
| Normal product aggregate shutdown | 3 s (all extension shutdowns run concurrently) |
| Coordinated-signal extension-shutdown cap | 1.4 s in interactive, plain, print, and host modes; then force-kill registered process groups |

Product discovery reads a selected extension manifest through the resource
resolver's 256 KiB bound. The lower-level `ExtensionManifest::load` API instead
defaults to 64 KiB; the product discovery path calls `parse` on the resolver's
bounded text and does not use that lower-level default.

---

The host uses one bounded writer to serialize complete frames onto the child's
stdin; the SDK likewise gives one writer sole ownership of extension stdout.
Neither abandons a partially written frame. API `0.2` requests are also
admitted through the negotiated concurrency semaphore.

---

## 1. Host-to-extension methods

### 1.1 `initialize`

The **first** host request, sent immediately after the child process starts.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "api_version": "0.1",
    "ygg_version": "0.5.0",
    "extension": {
      "name": "hello-world",
      "version": "0.1.0",
      "manifest_path": "/home/user/.ygg/extensions/hello-world/extension.toml",
      "source": "global"
    },
    "workspace": "/home/user/project",
    "capabilities": {
      "filesystem": "none",
      "process": false,
      "network": false
    },
    "contributes": {
      "tools": ["hello_world"],
      "commands": ["hello"],
      "hooks": ["before_prompt", "after_response"],
      "ui": ["status"],
      "context": true,
      "tool_renderers": ["hello_world"],
      "notifications": true,
      "confirmations": false
    },
    "host": {
      "session_id": null,
      "session_name": null,
      "model": "claude-sonnet-4-6",
      "reasoning": null,
      "active_skills": []
    }
  }
}
```

**API `0.1` response:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "api_version": "0.1",
    "tools": [
      {
        "name": "hello_world",
        "description": "Greet someone from an executable extension",
        "parameters": {
          "type": "object",
          "properties": {
            "name": { "type": "string" }
          },
          "required": ["name"],
          "additionalProperties": false
        }
      }
    ],
    "commands": [
      {
        "name": "hello",
        "description": "Show a greeting notification",
        "usage": "/hello [name]"
      }
    ]
  }
}
```

**Common validation rules:**

- `api_version` in the response must exactly match the manifest-selected
  version, or the child is rejected.
- The duplicate-free sets of `tools` and `commands` returned must exactly equal
  the corresponding names declared in `contributes`. Order does not matter;
  omissions, additions, and duplicate names all reject initialization.
- Each tool must have a non-empty `description` and `parameters` must be a
  JSON Schema object.
- `source` is one of `"project"`, `"global"`, or `"explicit"`.

API `0.1` uses the request and response above and must omit `protocol` from
both. A `0.1` tool definition cannot declare `output_schema`.

API `0.2` retains every top-level initialization field and adds this request
member:

```json
{
  "protocol": {
    "version": "0.2",
    "required_features": ["request_cancellation", "content_parts"],
    "optional_features": [
      "request_progress",
      "artifacts",
      "lifecycle_events",
      "policy_intents",
      "dynamic_tools"
    ],
    "limits": {"max_concurrent_requests": 64}
  }
}
```

The response must retain top-level `api_version`, `tools`, and `commands`, and
add:

```json
{
  "protocol": {
    "version": "0.2",
    "features": [
      "request_cancellation",
      "content_parts",
      "request_progress",
      "artifacts",
      "lifecycle_events",
      "dynamic_tools"
    ],
    "limits": {"max_concurrent_requests": 4},
    "lifecycle_events": [
      "session/started",
      "session/settled",
      "turn/started",
      "turn/settled",
      "tool/started",
      "tool/settled"
    ]
  }
}
```

`request_cancellation` and `content_parts` are required. The response may add
any subset of the advertised optional features, but missing required,
unknown, or duplicate feature names reject initialization. The accepted
`max_concurrent_requests` must be greater than zero and is capped by the host.
If `lifecycle_events` is negotiated and the subscription list is omitted or
empty, all six events are subscribed. Otherwise it must be an exact subset of
the six names above. A non-empty subscription without the feature is invalid.

The working-tree coding host conditionally appends `agent_sessions` to
`optional_features` only when bounded V2 delegation is available for the
selected model/reasoning mode. A response may negotiate it only when it was
offered. The service is bound after the Agent is constructed; calls without a
bound service/resource owner fail deterministically with `-32002`.

The host likewise appends `approvals` only when single-use approval issuance is
enabled, and appends `secrets` only when a secret broker is configured and the
manifest's exact `[capabilities].secrets` allowlist is non-empty. Negotiating
`approvals` also requires `policy_intents`; neither conditional service may be
returned when it was not offered. The coding product currently leaves
approvals disabled, configures no secret broker, and supervises generic
`policy/evaluate` requests with `deny`, so it offers neither conditional
feature.

Secret names are duplicate-free identifiers of at most 64 ASCII bytes. The
first character is a letter or underscore; subsequent characters may also use
digits, hyphen, or dot. The list is a broker allowlist, not launch-environment
injection, and exact undeclared names remain inaccessible. An empty allowlist is
omitted from the serialized `capabilities` object, preserving the literal API
`0.1` initialize shape above. A non-empty current/API `0.2` manifest includes
the exact `secrets` array in that object.

API `0.2` tool definitions may add `output_schema`, a bounded supported subset
of JSON Schema used to validate `structured_content`. Schema nodes must be
objects, nesting is capped at 32, property names at 256 bytes, and the accepted
keywords are `$schema`, `title`, `description`, `default`, `examples`, `type`,
`properties`, `required`, `additionalProperties`, `items`, `enum`, `const`,
`allOf`, `anyOf`, `oneOf`, `minimum`, `maximum`, `exclusiveMinimum`,
`exclusiveMaximum`, `minLength`, `maxLength`, `minItems`, `maxItems`,
`uniqueItems`, `minProperties`, and `maxProperties`. Arbitrary JSON Schema
vocabulary is rejected.

---

### 1.2 `tool/call`

Invoke a model-callable tool.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tool/call",
  "params": {
    "name": "git_status",
    "arguments": {
      "include_ignored": false,
      "max_entries": 80
    },
    "catalog_revision": 2,
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "resource_owner": {
        "session_id": "durable-session-owner",
        "extension_instance_id": "host-created-instance-fence",
        "process_generation": 3
      },
      "host": {
        "session_id": "abc123",
        "session_name": null,
        "model": "claude-sonnet-4-6",
        "reasoning": null,
        "active_skills": []
      }
    }
  }
}
```

`catalog_revision` is present only for API `0.2` extensions that negotiated
`dynamic_tools`. It selects the exact per-process catalog epoch the model saw.
An extension must dispatch against that historical schema-and-handler snapshot,
not whatever catalog happens to be newest when the call arrives. API `0.1` and
API `0.2` without `dynamic_tools` omit the field.

`resource_owner` is present for API `0.2` model-tool and tool-hook contexts.
Its host-derived `session_id` is the durable namespace for extension state.
`extension_instance_id` changes across a complete process-host rebuild, even
when generation numbering restarts, and `process_generation` rejects stale
browser tabs, MCP/LSP connections, memory handles, and comparable resources
after an extension reload or automatic restart within that host instance. Key
state by all three fields. API `0.1` omits the field. Other contribution
contexts currently omit it and must not allocate session-owned resource
handles.

**API `0.1` response:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": "branch=main\nstate=clean\ncounts=staged:0,modified:0,untracked:0,ignored:0,conflicted:0",
    "is_error": false
  }
}
```

**Fields:**
- `content` — compact model-visible result text (string).
- `is_error` — if `true`, the result is treated as a tool error.
- `metadata` — optional JSON accepted by the API `0.1` decoder for compatibility.
  The current subprocess adapter discards it while constructing native
  `ToolOutput`; it is not sent to the model and has no frontend, renderer, or
  persistence guarantee.

**API `0.2` response:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": [
      {"type": "text", "text": "Found 3 sources."},
      {
        "type": "image",
        "artifact_id": "artifact-opaque-id",
        "mime_type": "image/png",
        "alt": "Search result preview"
      }
    ],
    "structured_content": {
      "sources": [{"title": "Example", "url": "https://example.com"}]
    },
    "is_error": false,
    "metadata": {"cache": "miss"}
  }
}
```

API `0.2` `content` is a non-empty ordered array of at most 256 parts and must
contain at least one explicit text part. Supported parts are:

- `{ "type": "text", "text": string }`
- `{ "type": "image", "artifact_id": string, "mime_type": string,
  "alt"?: string }`
- `{ "type": "audio", "artifact_id": string, "mime_type": string,
  "transcript"?: string }`

Image and audio parts require the `artifacts` feature and must name a verified
artifact from the active host-derived session owner and process generation
whose MIME type and media kind match the part. Repeated references count toward
a 64 MiB aggregate referenced media bound per result. If the tool declares
`output_schema`, `structured_content` is required and validated; without an
output schema it is forbidden. Structured content and bounded metadata are
retained in native result details. Structured content is lowered to the model
only by explicit host policy; metadata remains non-model-visible. Text and
verified media use the normal native tool-output path. Structured content is
bounded to 256 KiB; metadata to 64 KiB; both are
capped at 32 levels and 16,384
nodes, and metadata keys are at most 256 bytes.

---

### 1.3 `command/execute`

Invoke a slash command (`/mycommand`).

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "command/execute",
  "params": {
    "name": "checkpoint",
    "arguments": ["release-v2"],
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "host": {}
    }
  }
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "text": "Checkpoint preview · release-v2\nmain · clean\n\n...",
    "notifications": [
      {
        "level": "info",
        "title": "Read-only checkpoint",
        "message": "No commit or filesystem mutation was performed."
      }
    ],
    "context": []
  }
}
```

---

### 1.4 `hook/run`

Invoke a lifecycle hook (declared in `contributes.hooks`).

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "hook/run",
  "params": {
    "hook": "before_prompt",
    "payload": { "prompt": "the user's current input text..." },
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "host": {}
    }
  }
}
```

`hook` is one of: `"before_prompt"`, `"after_response"`, `"before_tool_call"`, `"after_tool_call"`.

`payload` is hook-specific:
- `before_prompt`: `{ "prompt": string }`
- `after_response`: `{ "response": string }`
- `before_tool_call`: `{ "name": string, "arguments": { ... } }`
- `after_tool_call`: `{ "name": string, "arguments": { ... }, "output": string, "is_error": bool }`

API `0.1` invokes `after_response` only after a successful, complete assistant
response. It is not a terminal-run hook and is not invoked for failure,
cancellation, interruption, frontend loss, or shutdown.

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "disposition": { "action": "continue" },
    "context": [
      {
        "label": "local-model-workflow",
        "content": "Local-model workflow is active...",
        "placement": "system_suffix"
      }
    ],
    "notifications": []
  }
}
```

**Dispositions:**
- `{ "action": "continue" }` — proceed normally.
- `{ "action": "deny", "reason": "..." }` — deny the intercepted operation
  (meaningful for `before_prompt` and `before_tool_call` only; other hooks
  continue regardless).

---

### 1.5 `context/collect`

Host requests prompt context contributions.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "context/collect",
  "params": {
    "prompt": null,
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "host": {}
    }
  }
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": [
    {
      "label": "hello-world",
      "content": "The hello-world extension is active.",
      "placement": "system_suffix"
    }
  ]
}
```

`placement` is one of: `"system_prefix"`, `"system_suffix"`, `"prompt_prefix"`, `"prompt_suffix"` (default).

---

### 1.6 `status/collect`

Host requests a TUI status/header/footer contribution.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "status/collect",
  "params": {
    "surface": "status",
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "host": {}
    }
  }
}
```

`surface` is one of: `"status"`, `"header"`, `"footer"`.

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "surface": "status",
    "text": "hello",
    "style_role": "extension.hello_world.status",
    "priority": 0
  }
}
```

Return `null` to contribute nothing.

---

### 1.7 `tool/render`

Host requests semantic renderer output for a tool call.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "tool/render",
  "params": {
    "name": "git_status",
    "arguments": {},
    "output": "branch=main\nstate=clean\n...",
    "is_error": false,
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
      "host": {}
    }
  }
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "segments": [
      { "text": "git · clean", "style_role": "extension.git_tools.clean" },
      { "text": "\n", "style_role": null },
      { "text": "...", "style_role": "extension.git_tools.detail" }
    ]
  }
}
```

---

### 1.8 `shutdown`

Sent when the host wants the extension to exit gracefully.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "shutdown",
  "params": {}
}
```

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": {}
}
```

On normal shutdown, the host first waits `shutdown_timeout` (2 seconds by
default) for the JSON-RPC reply. Whether the request is acknowledged or times
out, it then waits up to the same per-stage timeout for the child to exit. If
the child does not exit, Ygg terminates the child process group. Normal product
shutdown runs these per-connection sequences concurrently inside a separate
3-second aggregate deadline; dropping a remaining connection also terminates
its process group.

Interactive, plain, print, and host coordinated-signal exits instead cap the
whole extension-shutdown attempt at 1.4 seconds, then force-kill all registered
process groups. That outer cap may cut either normal 2-second stage short.
Closing stdin is not the graceful shutdown signal; it is a
transport-loss/final-teardown fallback.

API `0.2` enters `draining` before shutdown: it admits no new work, cancels
remaining requests at the bounded deadline, then performs the same bounded
request/ack, exit, and process-group cleanup sequence. API `0.1` keeps its
legacy resident-process behavior.

---

### 1.9 `$/cancelRequest` (API `0.2`)

The host cooperatively cancels a host request, or an extension-originated child
request owned by it:

```json
{
  "jsonrpc": "2.0",
  "method": "$/cancelRequest",
  "params": {"id": 42, "reason": "user"}
}
```

Before the serialized writer starts the original frame, cancellation skips the
frame and sends nothing. Once writing has begun, that complete frame is sent,
followed by at most one cancellation notification. A cooperative extension
observes its request token and answers the original ID with JSON-RPC error code
`-32800`; a normal result may instead win the race.

The host drops the cancelled waiter, cancels its unresolved child requests,
and tombstones the ID. A late response to a tombstone is ignored and diagnosed
without closing unrelated calls. If the extension does not settle within the
2-second default grace period, its generation becomes degraded and is
terminated. Cancellation requests cooperation; it does not imply rollback of
external side effects and an ambiguous unsafe operation is never replayed.

---

### 1.10 Lifecycle notifications (API `0.2`)

When `lifecycle_events` and the specific method are subscribed, the host sends
best-effort JSON-RPC notifications named:

- `session/started`, `session/settled`
- `turn/started`, `turn/settled`
- `tool/started`, `tool/settled`

For example:

```json
{
  "jsonrpc": "2.0",
  "method": "turn/settled",
  "params": {
    "session_id": "abc123",
    "run_id": "extension-run-7",
    "turn_id": "extension-turn-7",
    "outcome": "completed",
    "duration_ms": 942,
    "reason": null
  }
}
```

Start events carry their stable session/run/turn identifiers. Tool events also
carry `tool_call_id` and `tool_name`. Settled events add `outcome`,
`duration_ms`, and an optional `reason` bounded to 4 KiB UTF-8. Outcomes are
`completed`,
`failed`, `cancelled`, `interrupted`, `frontend_disconnected`, `shutdown`, or
`limit_reached`. These observations are non-veto and bounded: product
session/turn delivery uses a 250 ms deadline and tool observations use the
bounded writer queue. Host finalizers remain authoritative if delivery fails.

API `0.1` `after_response` remains success-only. It is not sent on failure,
cancellation, interruption, frontend loss, shutdown, or a turn limit.

---

## 2. Extension-to-host messages

Extensions send these after `initialize` completes. API `0.2` operation-scoped
requests (`confirmation/request`, `input/request`, `artifact/publish`,
`policy/evaluate`, and `secret/get`, plus every `agent/*` method) must include
the active numeric host `parent_request_id`.
Global notifications, context and status contributions, and process-scoped
tool-catalog mutations do not include it. When the parent settles, the host
cancels every unresolved operation-scoped child and ignores late child replies.

Every extension-originated request ID is either an unsigned 64-bit integer or
a string of at most 256 UTF-8 bytes. IDs are unique among outstanding child
requests and cannot be reused within a process generation. At most 128 child
requests may be outstanding and at most 65,536 distinct child IDs may be used
in one generation.

### 2.1 `notification`

Emit a user-visible notification.

```json
{
  "jsonrpc": "2.0",
  "method": "notification",
  "params": {
    "level": "success",
    "title": "Hello",
    "message": "hello_world greeted tinkerer"
  }
}
```

`level` is one of: `"info"` (default), `"success"`, `"warning"`, `"error"`.

This is a JSON-RPC **notification** (no `id`); the host does not reply.

---

### 2.2 `confirmation/request`

Ask the user a yes/no question.

```json
{
  "jsonrpc": "2.0",
  "id": "confirm-1",
  "method": "confirmation/request",
  "params": {
    "parent_request_id": 2,
    "prompt": "Push to origin/main?",
    "detail": "5 commits, +120 −30 lines",
    "destructive": false,
    "default": false
  }
}
```

The host **answers** the same `id`:

```json
{
  "jsonrpc": "2.0",
  "id": "confirm-1",
  "result": {
    "confirmed": true
  }
}
```

- The `id` must be a string no longer than 256 UTF-8 bytes, or an unsigned
  64-bit integer.
- `parent_request_id` is required and must name an active host request in API
  `0.2`; frozen API `0.1` omits it.
- The extension **must** wait for the answer before proceeding with the
  confirmed action.
- Dropping the request or using a non-interactive frontend denies it.
- Pending confirmation IDs carry a process generation; they cannot be
  answered against a replacement child after reload.

---

### 2.3 `context/contribution`

Unsolicited prompt context pushed by the extension at any time.

```json
{
  "jsonrpc": "2.0",
  "method": "context/contribution",
  "params": {
    "label": "file-watcher",
    "content": "src/main.rs was modified at 14:32.",
    "placement": "system_suffix"
  }
}
```

Notification (no `id`).

---

### 2.4 `status/contribution`

Unsolicited TUI surface contribution.

```json
{
  "jsonrpc": "2.0",
  "method": "status/contribution",
  "params": {
    "surface": "status",
    "text": "watching src/",
    "style_role": "extension.watcher.status",
    "priority": 10
  }
}
```

Notification (no `id`).

---

### 2.5 `$/progress` (API `0.2`)

Requires `request_progress`. Progress is correlated to one active host request
and sequences must increase strictly for that request:

```json
{
  "jsonrpc": "2.0",
  "method": "$/progress",
  "params": {
    "request_id": 2,
    "sequence": 7,
    "event": {
      "type": "status",
      "message": "Fetched 3 of 10 results",
      "current": 3,
      "total": 10,
      "unit": "results"
    }
  }
}
```

Event variants are:

- `status {message, current?, total?, unit?}`
- `output {stream: "stdout"|"stderr", encoding: "utf8"|"base64", data}`

Inactive-request and non-monotonic progress is ignored with a diagnostic.
Accepted output uses Ygg's existing 8 KiB chunking and bounded progress sink,
which may coalesce/drop under pressure. Progress is ephemeral: it is not a
model result and is not persisted in the conversation transcript.

---

### 2.6 `input/request` (API `0.2`)

Request ephemeral text from the frontend while an operation is active:

```json
{
  "jsonrpc": "2.0",
  "id": "input-1",
  "method": "input/request",
  "params": {
    "parent_request_id": 2,
    "prompt": "Password:",
    "secret": true
  }
}
```

The host answers the same ID with `{ "value": string|null }`. `null` means the
frontend cancelled or could not answer. Prompts must contain non-whitespace
text and are bounded to 16 KiB UTF-8; answers are bounded to 256 KiB UTF-8; the
1 MiB full-frame bound also applies. `secret: true` suppresses echo and ordinary
editor handling in an interactive frontend. Secret answers stay on the private
reply channel and are never placed in diagnostics, progress, session state, or
persistence. Headless/unavailable input is cancelled rather than guessed.
Parent settlement cancels the pending input request.

---

### 2.7 `artifact/publish` (API `0.2`)

Requires `artifacts`. Publish either small inline base64 data:

```json
{
  "jsonrpc": "2.0",
  "id": "artifact-1",
  "method": "artifact/publish",
  "params": {
    "parent_request_id": 2,
    "mime_type": "image/png",
    "size": 1234,
    "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "data": {"encoding": "base64", "data": "..."}
  }
}
```

or a relative path under `YGG_EXTENSION_SCRATCH` by replacing `data` with
`"path": "screenshots/result.png"`. Exactly one source is required. Success
returns `{ "artifact_id": "..." }`; malformed or rejected publication uses
`-32602`, while a request without an active host-owned session context uses
`-32002`.

The host securely reads a regular no-follow scratch file, snapshots immutable
bytes, and verifies exact size, lowercase SHA-256, canonical MIME, and media
signature. Defaults are 256 KiB inline, 20 MiB per artifact, 64 MiB and 64
artifacts per process generation, and a 4096-byte/64-component relative path.
The 1 MiB JSON-line bound still applies. Supported media are PNG, JPEG, GIF,
WebP, WAV, MPEG audio, FLAC, Opus, AAC, and MP4 audio. IDs are opaque and valid
only for the host-derived session owner that published them and that process
generation. Tool-result media resolution supplies the same owner internally;
another owner sees an unknown artifact even if it learns the opaque ID. Reload,
generation settlement, or an owner mismatch makes the handle unavailable.

---

### 2.8 `policy/evaluate` (API `0.2`)

Requires `policy_intents`:

```json
{
  "jsonrpc": "2.0",
  "id": "policy-1",
  "method": "policy/evaluate",
  "params": {
    "parent_request_id": 2,
    "intent": {
      "kind": "external_side_effect",
      "operation": "browser.submit_form",
      "target": {"origin": "https://example.com", "label": "Publish comment"},
      "data_classes": ["user_text"],
      "adapter_hints": {"read_only": false, "destructive": false}
    }
  }
}
```

The response is `{ "decision": "allow"|"ask"|"deny",
"approval_token"?: string }`. The host owns classification. Adapter hints can
only increase caution and do not authorize an action. `confirmation/request`
remains cooperative UI, not policy enforcement.

The optional `approvals` feature adds a single-use retry boundary. If the host
classifies an intent as `ask` and a trusted frontend approves it, the response
remains `ask` but includes a 64-character lowercase hexadecimal
`approval_token`. A token is valid only on `ask`; `allow` and `deny` responses
must omit it. The extension must repeat `policy/evaluate` with the exact same
`intent`, still-active `parent_request_id`, and that token:

```json
{
  "jsonrpc": "2.0",
  "id": "policy-2",
  "method": "policy/evaluate",
  "params": {
    "parent_request_id": 2,
    "intent": {
      "kind": "external_side_effect",
      "operation": "browser.submit_form",
      "target": {"origin": "https://example.com", "label": "Publish comment"},
      "data_classes": ["user_text"],
      "adapter_hints": {"read_only": false, "destructive": false}
    },
    "approval_token": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  }
}
```

The host atomically consumes the token and returns `allow` only when it still
matches the canonical original intent, active owner/parent, and process
generation before its bounded expiry (at most five minutes). Expiry, reuse, or
intent/parent/generation mismatch returns `deny`; a recognized mismatched token
is consumed as well. Supplying a token without negotiated `approvals` is
rejected with `-32602`. Approval capability state is invalidated on generation
replacement. The coding product currently has approvals off and no domain
policy adapter, so its policy supervisor returns `deny` without a token.

An extension may send `$/cancelRequest` for one of its own outstanding child
request IDs. The host also sends it automatically when the owning parent
settles.

---

### 2.9 `secret/get` (API `0.2`)

Requires the conditionally offered `secrets` feature and an exact name from the
manifest's `[capabilities].secrets` allowlist:

```json
{
  "jsonrpc": "2.0",
  "id": "secret-1",
  "method": "secret/get",
  "params": {
    "parent_request_id": 2,
    "name": "browser.api_token"
  }
}
```

Success returns `{ "value": string }`. Values are UTF-8, may be empty, and are
capped at 64 KiB. Ygg derives the extension identity and the complete
`{session_id, extension_instance_id, process_generation}` resource owner from
the active parent rather than accepting either from child JSON. The broker
receives that identity, owner, parent request ID, and exact logical name.

An undeclared/invalid name returns `-32602`; no active owner or service returns
`-32002`. A broker returning no value or failing always returns the same
`-32004` `secret is unavailable` response, keeping provider details host-side.
Ygg does not persist or log the value and best-effort wipes the host broker
buffer and serialized writer frame after use. Once delivered, however, the
extension holds an ordinary process-memory string: API `0.2` does not promise
end-to-end zeroization. The coding product currently configures no broker and
therefore does not offer `secrets`.

---

### 2.10 `tools/register` (API `0.2`)

Requires `dynamic_tools`. Add new tools or replace complete definitions for
existing extension-owned names:

```json
{
  "jsonrpc": "2.0",
  "id": "catalog-1",
  "method": "tools/register",
  "params": {
    "tools": [{
      "name": "ableton_tracks",
      "description": "List tracks in the active Ableton set",
      "parameters": {"type": "object", "properties": {}},
      "output_schema": null
    }]
  }
}
```

The request contains complete `ToolDefinition` objects, not patches. Names in
one request must be unique. The request and complete prospective catalog are
capped at 256 tools. Ygg validates the complete result, reserves names against
host tools and other extensions, applies the active host tool policy, and
publishes the extension group atomically. After a parseable request ID,
malformed parameters, a schema error, duplicate, or name conflict return
`-32602` and leave the previously published group unchanged.

Success returns the new per-process epoch and the complete set Ygg accepted:

```json
{
  "jsonrpc": "2.0",
  "id": "catalog-1",
  "result": {
    "revision": 1,
    "tools": ["ableton_tracks", "ableton_transport"]
  }
}
```

The returned list is authoritative: policy may omit requested tools. Revision
`0` is the initialize catalog, and every accepted mutation increments it once.
The epoch resets to `0` for a new process generation. If acknowledgement cannot
be delivered after publication, Ygg removes the extension's dynamic group and
terminates the generation so host and extension cannot continue with different
catalogs.

---

### 2.11 `tools/unregister` (API `0.2`)

Requires `dynamic_tools`. Remove extension-owned names transactionally:

```json
{
  "jsonrpc": "2.0",
  "id": "catalog-2",
  "method": "tools/unregister",
  "params": {"names": ["ableton_tracks"]}
}
```

Names must be unique valid tool identifiers; missing names are ignored. The
same 256-name bound, revision rules, transactional publication, authoritative
response shape, and acknowledgement-failure handling as `tools/register`
apply. The Python SDK rejects an empty mutation locally; callers should not use
no-op catalog changes as revision clocks.

The exact initialize catalog is authoritative epoch `0` and is the only
deterministic first-request catalog. Once a post-initialize catalog change is
accepted, Ygg advertises it at the next model-request boundary after
publication. It does not infer catalog quiescence, so a registration sent
immediately after `initialize` is not guaranteed to appear on turn one; put
turn-one tools in the manifest and initialize response. A provider request
already in flight retains the schema and tool implementation it was given.
Calls therefore carry `tool/call.catalog_revision`; extensions must keep enough
historical dispatch state for overlapping turns and reject an unknown or
retired epoch with `-32602`. The Python SDK retains eight committed catalog
snapshots and temporarily exposes its staged next epoch during the
publication-before-ack window. A replacement generation likewise begins from
its initialize catalog at epoch `0`; its subsequent mutations obey the same
next-boundary rule.

---

### 2.12 `agent/spawn` (API `0.2`, working tree)

Requires the conditionally offered `agent_sessions` feature and an active
host-tool parent. Create one bounded in-harness child model session:

```json
{
  "jsonrpc": "2.0",
  "id": "agent-1",
  "method": "agent/spawn",
  "params": {
    "parent_request_id": 2,
    "task_name": "inspect-midi-tools",
    "message": "Inspect the current Ableton MCP catalog.",
    "idempotency_key": "catalog-audit-2026-08-16"
  }
}
```

The host derives the resource owner from `parent_request_id`; the extension
cannot submit an owner. The idempotency key is 1..=256 bytes and scoped to the
extension principal plus that owner. Retrying identical `task_name`/`message`
returns the same result. Reusing the key with different input fails with
`-32002`. `task_name` is 1..=48 lowercase ASCII letters, digits, underscores,
or hyphens. The message is capped at 128 KiB. Success includes `agent_id`,
`agent_path`, caller-visible `task_name`, `status`, extension `principal`, and
the durable session `resource_owner` string.

---

### 2.13 `agent/message` (API `0.2`, working tree)

Send steering input to an owned child while preserving the child session:

```json
{
  "jsonrpc": "2.0",
  "id": "agent-2",
  "method": "agent/message",
  "params": {
    "parent_request_id": 2,
    "target": "agent-1",
    "message": "Also check resource tools."
  }
}
```

Success returns `{ "delivered_to": string, "delivery": "steering"|"queued"
}`. The target may be the ID or path returned by `agent/spawn`, but must belong
to this extension principal and resource owner's child-session trees. The
message is capped at 128 KiB.

---

### 2.14 `agent/follow_up` (API `0.2`, working tree)

Queue a subsequent run on an owned child:

```json
{
  "jsonrpc": "2.0",
  "id": "agent-3",
  "method": "agent/follow_up",
  "params": {
    "parent_request_id": 2,
    "target": "agent-1",
    "message": "Now summarize only callable tools."
  }
}
```

Success returns `agent_id`, `agent_path`, and `delivery` (`follow_up` when the
child is running, otherwise `new_run`). Ownership checks match
`agent/message`; the follow-up is capped at 128 KiB.

---

### 2.15 `agent/list` (API `0.2`, working tree)

Request `{ "parent_request_id": 2 }`. Success returns the extension
`principal`, derived `resource_owner`, `persistence_error`, and `agents`. Each
agent record contains `agent_id`, `agent_path`, `parent_id`, `task_name`,
`depth`, durable `session` path, and tagged `status`. States are `pending`,
`running`, `completed` (with bounded `output`), `interrupted`, `failed` (with
bounded `error`), and `shutdown`. The list contains only roots spawned by this
principal/owner and their descendants.

---

### 2.16 `agent/wait` (API `0.2`, working tree)

Request `{ "parent_request_id": 2, "timeout_ms": 30000 }`. The timeout
defaults to 30 seconds and is clamped to 1..=60,000 ms. The request returns
immediately when no owned child is pending/running; otherwise it waits until all
owned children settle or the deadline expires. Success is
`{ "timed_out": bool, "snapshot": <agent/list result> }`. Parent settlement
cancels the wait.

---

### 2.17 `agent/interrupt` (API `0.2`, working tree)

Request `{ "parent_request_id": 2, "target": "agent-1" }`. Success returns
`agent_id`, `agent_path`, `previous_status`, and `interrupt_requested`. The host
cancels the owned descendant tree when an active interrupt is requested.

All six methods share the child-request and eight-worker bounds. After a
parseable request ID, malformed parameters return `-32602`. Unavailable
service/owner, invalid ownership, exhausted delegation limits, persistence
failure, or an invalid operation return `-32002`. The extension's stable
principal is derived from its manifest name and path; a different extension or
resource owner cannot list, message, follow up, wait on, or interrupt its child
trees. Ownership uses that principal plus the durable session-owner string, not
the extension process generation, so supervised restart/reload can resume an
existing tree. Process shutdown requests shutdown of the service's owned trees;
a complete process-host rebuild creates a new service boundary. Hosted agents
are a separate capability; these methods create Ygg child conversations.
Observe their state through `agent/list`/`agent/wait`. Delegated child turns do
not currently emit extension `session/*` or `turn/*` lifecycle notifications;
that notification stream covers the owning/root product session.

---

## 3. Standard JSON-RPC errors

| Code | Message | Meaning |
|---|---|---|
| `-32700` | Parse error | Invalid JSON was received |
| `-32600` | Invalid Request | JSON is not a valid Request object |
| `-32601` | Method not found | Method does not exist / is not implemented |
| `-32602` | Invalid params | Method arguments are invalid |
| `-32603` | Internal error | Internal JSON-RPC error |
| `-32800` | Request cancelled | API `0.2` cooperative cancellation won |
| `-32000` to `-32099` | Server error | Reserved for implementation-defined errors |

Extensions should use `-32601` for unknown methods and `-32602` for invalid
parameters. Custom errors in the `-32000` to `-32099` range are reserved for
extension-specific server errors.

---

## 4. Type reference

### `ExtensionIdentity`

| Field | Type | Description |
|---|---|---|
| `name` | string | Stable extension name (matches manifest) |
| `version` | string | Semantic version |
| `manifest_path` | string | Absolute path to `extension.toml` |
| `source` | string | `"project"`, `"global"`, or `"explicit"` |

### `ExtensionHostState`

| Field | Type | Description |
|---|---|---|
| `session_id` | string \| null | Stable session identifier |
| `session_name` | string \| null | User-assigned session name |
| `model` | string \| null | Canonical current model ID |
| `reasoning` | value \| null | Reasoning configuration |
| `active_skills` | array | Explicitly active skills |

### `ActiveSkill`

| Field | Type | Description |
|---|---|---|
| `id` | string | Stable skill identifier |
| `name` | string | Human-readable skill name |
| `version` | string \| null | Skill version |

### `ExtensionExecutionContext`

| Field | Type | Description |
|---|---|---|
| `workspace` | string | Active workspace root |
| `execution_scope` | string \| null | Tool execution scope ID |
| `resource_owner` | object \| null | API `0.2` host-derived `{session_id, extension_instance_id, process_generation}` for model-tool and tool-hook resource ownership; omitted elsewhere and in `0.1` |
| `host` | object | Current `ExtensionHostState` |

### `ExtensionResourceOwner` (API `0.2`)

| Field | Type | Description |
|---|---|---|
| `session_id` | string | SHA-256-derived durable canonical-session-path namespace; never supplied by model arguments |
| `extension_instance_id` | string | Host-created instance fence that changes across a complete process-host rebuild, including when generation numbering restarts |
| `process_generation` | unsigned integer | Reload/automatic-restart fence within one host instance for rejecting stale extension handles |

### `ToolDefinition`

| Field | Type | Description |
|---|---|---|
| `name` | string | Tool name; initialize names match the manifest, while negotiated `dynamic_tools` may publish post-initialize names |
| `description` | string | Model-facing description |
| `parameters` | object | JSON Schema (must be an object type) |
| `output_schema` | object \| null | API `0.2` schema for required `structured_content`; forbidden in `0.1` |

### `CommandDefinition`

| Field | Type | Description |
|---|---|---|
| `name` | string | Command name without leading `/` |
| `description` | string | User-facing summary |
| `usage` | string \| null | Compact usage string |

### `ContextContribution`

| Field | Type | Description |
|---|---|---|
| `label` | string | Stable label for context inspection |
| `content` | string | Plain text sent to the model |
| `placement` | string | `"system_prefix"`, `"system_suffix"`, `"prompt_prefix"`, `"prompt_suffix"` |

### `ExtensionNotification`

| Field | Type | Description |
|---|---|---|
| `level` | string | `"info"`, `"success"`, `"warning"`, `"error"` |
| `title` | string \| null | Concise title |
| `message` | string | Notification body |

### `ConfirmationRequest`

| Field | Type | Description |
|---|---|---|
| `parent_request_id` | unsigned integer | Active owning host request; required in API `0.2`, absent in `0.1` |
| `prompt` | string | Short action-oriented question |
| `detail` | string \| null | Additional scope detail |
| `destructive` | bool | Potentially destructive action |
| `default` | bool | Suggested default |

### `ExtensionInputRequest` (API `0.2`)

| Field | Type | Description |
|---|---|---|
| `parent_request_id` | unsigned integer | Active owning host request |
| `prompt` | string | Non-whitespace frontend prompt, at most 16 KiB UTF-8 |
| `secret` | bool | Suppress echo and ordinary editor handling |

The response contains exactly `value: string|null`, with non-null values capped
at 256 KiB UTF-8.

### `ExtensionSecretGetRequest` (API `0.2`)

| Field | Type | Description |
|---|---|---|
| `parent_request_id` | unsigned integer | Active owner-scoped host request |
| `name` | string | Exact manifest-allowlisted identifier, at most 64 ASCII bytes |

Success contains exactly `value: string`, capped at 64 KiB UTF-8. A broker
no-value result and provider failure share the generic `-32004` unavailable
result.

### `ToolRenderSegment`

| Field | Type | Description |
|---|---|---|
| `text` | string | Plain text content |
| `style_role` | string \| null | Semantic theme role |

### `StatusContribution`

| Field | Type | Description |
|---|---|---|
| `surface` | string | `"status"`, `"header"`, `"footer"` |
| `text` | string | Plain display text |
| `style_role` | string \| null | Semantic theme role |
| `priority` | int | Higher = retained first when constrained |

### API `0.2` protocol features

| Feature | Required | Enables |
|---|---|---|
| `request_cancellation` | yes | Cooperative `$/cancelRequest`, cancellation errors, tombstones |
| `content_parts` | yes | Ordered text/media tool-result parts and native result details |
| `request_progress` | no | Request-scoped `$/progress` |
| `artifacts` | no | `artifact/publish` and image/audio content parts |
| `lifecycle_events` | no | Subscribed session/turn/tool observations |
| `policy_intents` | no | Correlated `policy/evaluate` requests |
| `dynamic_tools` | no | Transactional `tools/register`, `tools/unregister`, and revision-pinned `tool/call` |
| `agent_sessions` | conditional | Principal/owner-scoped `agent/*` child model-session service |
| `approvals` | conditional | Original-intent/active-owner-bound single-use `policy/evaluate` retry tokens; also requires `policy_intents` |
| `secrets` | conditional | Owner-scoped `secret/get` for exact manifest-allowlisted names |

Parent correlation (including `input/request`), serialized writes, bounded
drain, and health tracking are base API `0.2` invariants rather than optional
feature strings.

---

## 5. Lifecycle

```
starting -> initializing -> ready -> draining -> stopped
                              |          |
                              +-> degraded/crashed
```

`/extensions` inspection exposes each running process generation, negotiated
features, pending request count, health state, and bounded last error. The
supervisor uses `backoff` after an unexpected exit or terminal transport failure
and `parked` after its retry budget or a permanent
manifest/version/re-registration error.

**Reload:** Ygg starts and fully negotiates candidate generation `N+1` while
`N` remains ready. A process negotiating `dynamic_tools` may replace its tool
catalog; otherwise changed tools, or any changed command/hook/UI contribution,
are rejected with `re-registration required` and require a full host rebuild.
Ygg reserves candidate tool names, marks `N` draining, waits the bounded drain,
cancels the remainder, emits remaining lifecycle terminals, and waits for
shutdown acknowledgement or timeout. It then seeds lifecycle state and
atomically routes new calls to `N+1`. If candidate launch or negotiation fails,
the old process remains active. Stale progress, child requests, confirmation
IDs, approvals, secret lookups, artifacts, catalog epochs, and resource handles
cannot cross the generation boundary. Unresolved unsafe calls are never
replayed.
`/extensions reload` replaces running children; general `/reload` rebuilds
discovery and the product boundary.

**Unexpected exit:** after one successful initialization, the supervisor removes
the dead generation's tool group, enters `backoff`, and attempts the same
candidate-first generation-checked reload. Delay uses full jitter with a 250 ms
exponential base and 30 s cap. Eight failed restart attempts park the extension;
30 seconds continuously ready resets that budget. Permanent
manifest/version/re-registration errors park immediately. Explicit shutdown
cancels supervision, and the reload lock prevents a supervisor/manual reload
race. A manual generation change revives the parked watcher. Initial
spawn/initialize failures are reported as generation-0 parked discovery entries
and are not retried by this post-initialization supervisor. The supervisor does
not heartbeat an otherwise live process. A full product rebuild creates a new
extension instance and supervisor, resetting in-memory retry/parked state.

API `0.1` has one resident contact policy and no manifest contact-policy field.
An enabled, trusted extension is started while the product extension host is
constructed only when the resolved product policy is UnsafeHost and the
independent process gate permits startup; Controlled retains discovery without
a resident child. Each admitted generation remains resident until reload,
shutdown, or connection failure; the host supervisor may create a replacement
after failure. On reload, the replacement is initialized before the active
generation stops admission and shuts down; the replacement is then swapped in.
Its wire contract remains frozen and does not emulate API `0.2` stateful
guarantees.

API `0.2` lifecycle notifications cover the shared terminal outcome boundary
used by interactive, plain, print, RPC, native-host, and Serve execution paths.
Every admitted turn is settled across completion, abort, failure, turn limit,
frontend stream loss, and shutdown. Observations do not own cleanup.

Sleep inhibition is not a protocol/kernel responsibility, and no core sleep
inhibitor remains. The example `caffeinate` package is an API `0.2`, version
`0.2.0` extension. It observes owning/root `turn/started`, `turn/settled`, and
`session/settled`, reference-counts overlapping turns, runs one bounded macOS
helper, and relies on extension shutdown plus process-group cleanup as its final
fence.

---

## 6. Environment variables

Every extension child receives:

| Variable | Value |
|---|---|
| `YGG_EXTENSION_API_VERSION` | Exact manifest-selected version (`"0.1"` or `"0.2"`) |
| `YGG_EXTENSION_NAME` | Extension manifest name |
| `YGG_EXTENSION_DIR` | Extension directory (beside manifest) |
| `YGG_EXTENSION_MANIFEST` | Absolute path to `extension.toml` |
| `YGG_WORKSPACE` | Active workspace root |
| `YGG_EXTENSION_SCRATCH` | Host-owned scratch directory for the active process generation |
