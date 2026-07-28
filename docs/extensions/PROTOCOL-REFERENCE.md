# Ygg Extension Protocol Reference

> **API version:** `0.1` (constant `EXTENSION_API_VERSION`)
>
> Every request and response uses the standard JSON-RPC 2.0 envelope with
> exactly one JSON object per line on **stdout**. Human diagnostics belong on
> **stderr**, which Ygg drains and exposes as bounded diagnostic events.
>
> Extensions send process-to-host messages at any time after initialization.
> The host closes stdin to signal shutdown; the extension should exit promptly.

## Transport defaults

| Parameter | Default |
|---|---|
| Max JSON line | 1 MiB (`DEFAULT_EXTENSION_MESSAGE_BYTES`) |
| In-flight requests | 64 (`DEFAULT_PENDING_REQUESTS`) |
| Request timeout | 30 s |
| Shutdown grace | 3 s |

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
    "ygg_version": "0.3.2-alpha",
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

**Response:**

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

**Negotiation rules:**
- `api_version` in the response must match the host version exactly, or the
  child is rejected.
- `tools` and `commands` returned must be a *superset of nothing* — every name
  declared in `contributes` must appear; the extension may return fewer fields
  (e.g. drop a command at runtime by omitting it), but it cannot add tools or
  commands not declared in the manifest.
- Each tool must have a non-empty `description` and `parameters` must be a
  JSON Schema object.
- `source` is one of `"project"`, `"global"`, or `"explicit"`.

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
    "context": {
      "workspace": "/home/user/project",
      "execution_scope": null,
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

**Response:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": "branch=main\nstate=clean\ncounts=staged:0,modified:0,untracked:0,ignored:0,conflicted:0",
    "is_error": false,
    "metadata": {
      "branch": "main",
      "clean": true,
      "counts": { "staged": 0, "modified": 0, "untracked": 0, "ignored": 0, "conflicted": 0 }
    }
  }
}
```

**Fields:**
- `content` — compact model-visible result text (string).
- `is_error` — if `true`, the result is treated as a tool error.
- `metadata` — optional structured data for frontend/renderer use; not sent
  to the model.

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
      "host": { ... }
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
    "context": { ... }
  }
}
```

`hook` is one of: `"before_prompt"`, `"after_response"`, `"before_tool_call"`, `"after_tool_call"`.

`payload` is hook-specific:
- `before_prompt`: `{ "prompt": string }`
- `after_response`: `{ "message_id": string }`
- `before_tool_call`: `{ "tool": string, "arguments": { ... } }`
- `after_tool_call`: `{ "tool": string, "output": string, "is_error": bool }`

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
    "context": { ... }
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
    "context": { ... }
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
    "arguments": { ... },
    "output": "branch=main\nstate=clean\n...",
    "is_error": false,
    "context": { ... }
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

The host waits `shutdown_timeout` seconds, then kills the process.

---

## 2. Extension-to-host messages

Extensions send these at any time after `initialize` completes.

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

- The `id` must be a string ≤ 64 bytes, or a number.
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

## 3. Standard JSON-RPC errors

| Code | Message | Meaning |
|---|---|---|
| `-32700` | Parse error | Invalid JSON was received |
| `-32600` | Invalid Request | JSON is not a valid Request object |
| `-32601` | Method not found | Method does not exist / is not implemented |
| `-32602` | Invalid params | Method arguments are invalid |
| `-32603` | Internal error | Internal JSON-RPC error |
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
| `host` | object | Current `ExtensionHostState` |

### `ToolDefinition`

| Field | Type | Description |
|---|---|---|
| `name` | string | Tool name (must match manifest declaration) |
| `description` | string | Model-facing description |
| `parameters` | object | JSON Schema (must be an object type) |

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
| `prompt` | string | Short action-oriented question |
| `detail` | string \| null | Additional scope detail |
| `destructive` | bool | Potentially destructive action |
| `default` | bool | Suggested default |

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

---

## 5. Lifecycle

```
┌─────────────────────────────────────────────────────┐
│ 1. Host starts child process                         │
│ 2. Host sends initialize                             │
│ 3. Extension responds with tools + commands          │
│ 4. Host validates response against manifest           │
│ 5. ┌──────────────────────────────────────────────┐  │
│    │ Normal operation:                             │  │
│    │   tool/call, command/execute, hook/run,       │  │
│    │   context/collect, status/collect,             │  │
│    │   tool/render                                 │  │
│    │ Extension-initiated:                           │  │
│    │   notification, confirmation/request,          │  │
│    │   context/contribution, status/contribution    │  │
│    └──────────────────────────────────────────────┘  │
│ 6. Host sends shutdown                               │
│ 7. Extension responds, exits                         │
│ 8. Host waits grace period, then kills if alive      │
└─────────────────────────────────────────────────────┘
```

**Reload:** A replacement process is fully initialized before the old one is
shut down. If the new child fails, the old process stays active. Pending
confirmation IDs carry a process generation and cannot be answered against
the replacement.

**Contact policies:**
- `permanent` — started at session start, kept alive.
- `on_demand` — started only when needed for a tool/command/hook call.
- `auto_permanent` — same as permanent but only if the binary exists on disk.
- `tool_execute` — started fresh for each tool call.

---

## 6. Environment variables

Every extension child receives:

| Variable | Value |
|---|---|
| `YGG_EXTENSION_API_VERSION` | `"0.1"` |
| `YGG_EXTENSION_NAME` | Extension manifest name |
| `YGG_EXTENSION_DIR` | Extension directory (beside manifest) |
| `YGG_EXTENSION_MANIFEST` | Absolute path to `extension.toml` |
| `YGG_WORKSPACE` | Active workspace root |
