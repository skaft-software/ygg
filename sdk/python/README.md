# ygg-extension-sdk

`ygg-extension-sdk` is the dependency-free Python SDK for Ygg's executable
extension protocol. It owns JSON-RPC 2.0 JSON-lines framing, flushes every
response, validates the `initialize` negotiation against the selected manifest,
and keeps diagnostics on structured stderr logs.

The SDK sits on Ygg's tiny-kernel boundary. Ygg owns model conversations,
JSON-RPC transport and supervision, session/tool-result persistence,
permissions/approvals, cleanup, and shared limits. The extension process owns
the capability: MCP, web search, browser use, computer use, memory, LSP,
subagent orchestration, caffeinate, or another replaceable domain. Generic host
services such as artifacts and child model sessions do not make the host a
domain manager.

API `0.1` remains the default for existing extensions. API `0.2` adds negotiated
concurrency, cooperative request cancellation, correlated progress and host
requests, ephemeral input, structured/media results, artifact publication,
lifecycle events, live tool catalogs, scoped child model sessions, and graceful
drain. It also supports conditionally offered single-use approval retries and
owner-scoped secret lookup. Select `api_version = "0.2"` in the manifest; Ygg
exports the matching `YGG_EXTENSION_API_VERSION` and the SDK follows it
automatically. Override the constructor explicitly only for a standalone
harness or test:

```python
ext = Extension(api_version="0.2", max_concurrent_requests=4)
```

Install it from a checkout:

```console
python3 -m pip install ./sdk/python
```

A minimal extension is:

```python
from ygg_extension import Extension

ext = Extension()

@ext.tool(name="hello_world", description="Greet someone")
def hello(args):
    name = args.get("name", "world")
    return {"content": f"Hello, {name}!"}

ext.run()
```

Every bootstrap tool present during initialization must also appear in the
extension manifest:

```toml
[contributes]
tools = ["hello_world"]
```

The host sends the manifest, workspace, and current session/model state in the
first `initialize` request. Tool and command decorators present at that point
must exactly match the manifest declarations; a mismatch is rejected during
the handshake instead of being silently advertised. Tools published later with
negotiated `dynamic_tools` are post-initialize catalog entries and need not be
listed in the bootstrap manifest. The initialize tool set is epoch `0` and the
only deterministic first-request catalog. Put every turn-one tool there; Ygg
does not wait for an implicit post-initialize registration-quiescence period.

## Contribution points

```python
@ext.command(name="checkpoint", description="Preview a checkpoint")
def checkpoint(arguments, context):
    return {"text": "..."}

@ext.hook("before_prompt")
def before_prompt(payload, context):
    return {"disposition": {"action": "continue"}}

@ext.context
def context(params):
    return [{"label": "example", "content": "...", "placement": "system_suffix"}]

@ext.status("status")
def status(params):
    return {"surface": "status", "text": "ready", "priority": 0}

@ext.renderer("hello_world")
def render(params):
    return {"segments": [{"text": "hello", "style_role": None}]}
```

Tool handlers receive `(arguments, context)` when they declare two parameters;
one-parameter handlers receive only `arguments`. Commands receive an argument
array, hooks receive their payload, and context/status/renderer handlers receive
their complete protocol parameter object. Each of those handlers may accept a
second ambient context argument.

The API `0.1` hook payloads are:

- `before_prompt`: `{"prompt": string}`
- `after_response`: `{"response": string}`
- `before_tool_call`: `{"name": string, "arguments": object}`
- `after_tool_call`: `{"name": string, "arguments": object, "output": string,
  "is_error": bool}`

`after_response` is success-only in API `0.1`: Ygg invokes it after a complete
assistant response, not after failed, cancelled, interrupted, disconnected, or
shutdown runs. Do not use it as the sole cleanup boundary.

Tool handlers may return `content`, `is_error`, and `metadata`. The current Ygg
API `0.1` subprocess adapter uses `content` and `is_error` but discards
`metadata`; do not rely on it reaching a frontend, renderer, or persisted result.

## API 0.2 negotiation and scheduling

An API `0.2` host with every optional service enabled may send this `protocol`
object in `initialize`:

```json
{
  "version": "0.2",
  "required_features": ["request_cancellation", "content_parts"],
  "optional_features": [
    "request_progress",
    "artifacts",
    "lifecycle_events",
    "policy_intents",
    "dynamic_tools",
    "agent_sessions",
    "approvals",
    "secrets"
  ],
  "limits": {"max_concurrent_requests": 4}
}
```

The SDK rejects an unsupported required feature, returns the supported subset,
and caps `max_concurrent_requests` at its configured local maximum. Unknown
optional features are ignored. The SDK currently supports
`request_cancellation`, `content_parts`, `request_progress`, `artifacts`,
`lifecycle_events`, `policy_intents`, `dynamic_tools`, `agent_sessions`,
`approvals`, and `secrets`. The host offers `agent_sessions` only when its
bounded V2 delegation service is available for the selected model/reasoning
mode. It offers `approvals` only when approval issuance is enabled, and
`secrets` only when a broker is configured and the manifest secret allowlist is
non-empty. `approvals` also requires `policy_intents`. The coding product
currently leaves approvals off, configures no secret broker, and answers
generic policy intents with `deny`, so neither conditional feature is offered
there.

The stdio reader only decodes and queues frames—it never invokes extension code.
Handlers run in a bounded worker pool with a separately bounded admission queue
(`max_pending_requests`, default 64), and a single dedicated writer owns stdout
and serializes complete frames. API `0.1` uses the same safe transport with a
single handler worker, preserving sequential behavior.

The negotiated values remain available after initialization:

```python
ext.negotiated_features       # frozenset[str]
ext.negotiated_concurrency    # int
```

## Dynamic tool catalogs

When `dynamic_tools` is negotiated, an initialized extension can add, replace,
or remove tools without restarting Ygg:

```python
def search_handler(args):
    return f"result for {args['query']}"

update = ext.register_tool(
    name="provider_search",
    description="Search the selected provider",
    parameters={
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
    },
    handler=search_handler,
)
# {"revision": 1, "tools": ["provider_search", ...]}

ext.unregister_tool("provider_search")
```

`register_tools([...])` applies several definitions in one `tools/register`
request; an existing name replaces both its schema and handler.
`unregister_tools(*names)` sends one `tools/unregister` request, and an already
absent name is harmless. Each definition is an object containing `name`,
`description`, callable `handler`, and optional `parameters` and
`output_schema`. A request and the complete live catalog are limited to 256
tools. A mutation accepted after initialization appears at the next
model-request boundary after publication. A request sent immediately after the
handshake is therefore not guaranteed to alter the first model request.

Both operations are transactional. The SDK stages the prospective local
catalog, waits for the host acknowledgement, and commits only the exact tool
names returned by the host. A rejected request leaves the active local catalog
unchanged. The response revision must be the next monotonic epoch or the SDK
raises a protocol error. `ext.tool_catalog_revision` exposes the last committed
epoch; it begins at `0` after initialization and after each process restart.
On reload, the replacement initialize response is again authoritative epoch
`0`; later mutations follow the same next-boundary rule.

Ygg freezes one tool schema-and-implementation set for each model request and
sends that epoch as `tool/call.catalog_revision`. To cover calls from an older
in-flight turn, the SDK retains the eight most recent committed catalogs. It
also makes a staged catalog addressable before the mutation acknowledgement is
read, because the host publishes immediately before queueing that
acknowledgement. Unknown or retired revisions fail with `-32602` instead of
silently dispatching to a newer handler.

## Child model sessions

When the host offers and the extension negotiates `agent_sessions`, a tool
handler can orchestrate bounded in-harness Ygg child conversations:

```python
from ygg_extension import text_content, tool_result

@ext.tool(name="orchestrate", description="Delegate a bounded investigation")
def orchestrate(args):
    child = ext.spawn_agent(
        task_name="inspect-catalog",
        message="Inspect the current provider tool catalog.",
        idempotency_key=f"catalog:{ext.request_id}",
    )
    ext.send_agent_message(child["agent_id"], "Include resource tools.")
    ext.follow_up_agent(child["agent_id"], "Return a compact summary.")
    settled = ext.wait_agents(timeout_ms=30_000)
    agents = ext.list_agents()
    if settled["timed_out"]:
        ext.interrupt_agent(child["agent_id"])
    return tool_result(
        text_content(f"Observed {len(agents['agents'])} owned child sessions.")
    )
```

The exact helpers are:

- `spawn_agent(*, task_name, message, idempotency_key,
  parent_request_id=...)`;
- `send_agent_message(target, message, *, parent_request_id=...)`;
- `follow_up_agent(target, message, *, parent_request_id=...)`;
- `list_agents(*, parent_request_id=...)`;
- `wait_agents(*, timeout_ms=30_000, parent_request_id=...)`; and
- `interrupt_agent(target, *, parent_request_id=...)`.

Inside a model-tool handler, the SDK supplies the ambient parent automatically.
Outside one, pass an explicit active model-tool `parent_request_id`. The current
host derives ownership only from API `0.2` model-tool calls, so command/context/
status handlers must not create child sessions. All methods require the
negotiated feature and validate basic response shapes. `wait_agents` accepts
1..=60,000 ms. Task names use 1..=48 lowercase ASCII letters, digits,
underscores, or hyphens; task, steering, and follow-up text is capped at
128 KiB. The SDK additionally rejects empty messages before sending them.

`spawn_agent` is retry-safe only through its required idempotency key. The key
is scoped by the host to the extension principal and derived resource owner;
repeating identical input returns the same child, while reusing the key with
different input fails. Malformed wire parameters with a parseable ID return
`-32602`; unavailable service/owner and rejected operations return `-32002`.
The host accepts targets only from this principal and owner's spawned trees. It
owns model sessions, persistence, inherited permissions, and delegation limits;
the extension owns orchestration policy. Child trees are keyed by extension
principal plus durable session owner rather than process generation, so a
supervised restart or reload can resume them. Explicit extension shutdown stops
the owned trees.
These helpers create local in-harness Ygg children, not hosted-agent jobs.
Use `list_agents` and `wait_agents` for child state: delegated child turns do
not currently arrive through the extension's `session/*` or `turn/*` lifecycle
handlers, which observe the owning/root product session.
The Python negotiation/helper tests are implemented; the Rust product's
cross-process host-service gates remain pending in this working tree.

## Cancellation and progress

Each API `0.2` request handler receives an ambient thread-safe cancellation
token. Poll it between side effects, wait on it during interruptible work, or
raise the standard cancellation error:

```python
from ygg_extension import current_cancellation

@ext.tool(name="fetch_many", description="Fetch several records")
def fetch_many(args):
    token = current_cancellation()
    for index, item in enumerate(args["items"]):
        token.raise_if_cancelled()
        fetch(item)
        ext.progress(
            message=f"Fetched {index + 1} of {len(args['items'])}",
            current=index + 1,
            total=len(args["items"]),
            unit="records",
        )
    return "done"
```

`ext.cancellation` and `ext.request_id` expose the same ambient values. A
cooperative cancellation settles the original request with JSON-RPC error
`-32800`. Cancellation is idempotent: a normal result that has already won the
terminal race remains the sole result.

`ext.progress(...)` emits `$/progress` with a sequence starting at 1 and
increasing independently for each request. It accepts either the status
convenience arguments above or an explicit event:

```python
ext.progress({
    "type": "output",
    "stream": "stderr",
    "encoding": "utf8",
    "data": "retrying request",
})
```

Progress requires the negotiated `request_progress` feature and an active host
request. It is ephemeral protocol traffic, not tool-result content.

## Structured results and artifacts

API `0.2` tool results use content parts. String returns are converted to one
text part for migration convenience; helpers make the full envelope explicit:

```python
from ygg_extension import image_content, text_content, tool_result

@ext.tool(
    name="capture",
    description="Capture a screenshot",
    output_schema={
        "type": "object",
        "properties": {"url": {"type": "string"}},
        "required": ["url"],
    },
)
def capture(args):
    artifact_id = ext.publish_artifact(mime_type="image/png", data=image_bytes)
    return tool_result(
        text_content("Captured the page."),
        image_content(artifact_id, "image/png", alt="Page after submit"),
        structured_content={"url": args["url"]},
        metadata={"cache": "miss"},
    )
```

`audio_content(..., transcript=...)` builds an audio part with the same
host-artifact boundary; images use optional `alt` text and audio uses an
optional `transcript`.
Content parts are limited to text and host artifact references; arbitrary local
paths and remote URLs are not accepted as media results. `structured_content`
and `metadata` are retained independently of compact model-visible text.

`publish_artifact` supports bounded inline bytes as shown above, or a relative
host-owned scratch path with explicit size and SHA-256:

```python
artifact_id = ext.publish_artifact(
    mime_type="image/png",
    path="captures/result.png",
    size=byte_count,
    sha256=digest,
)
```

The SDK rejects absolute and parent-traversing scratch paths. The host remains
authoritative: it opens scratch files safely, verifies type/size/digest, ingests
the bytes, and returns the opaque artifact ID. Publication requires the active
host-derived session owner, and the ID resolves only for that same owner and
process generation. Passing a leaked ID from another owner in an image/audio
result fails as an unavailable artifact.

An API `0.2` tool may declare `output_schema=` alongside its argument
`parameters=`. Ygg validates `structured_content` against that schema. API
`0.1` tools cannot declare an output schema.

## Parent correlation and lifecycle

In API `0.2`, handler-originated `request(...)` calls automatically include the
ambient `parent_request_id`. `confirm(...)`, `request_input(...)`,
`publish_artifact(...)`, `evaluate_policy(...)`, `get_secret(...)`, and the
agent-session helpers require that correlation; callers outside a handler must
pass `parent_request_id=` explicitly. API `0.1` frames remain unchanged.

API `0.2` model-tool and tool-hook handler contexts include
`context["resource_owner"]` with a durable host-derived `session_id`, an
`extension_instance_id`, and a `process_generation` fence. Use the complete
triple to namespace browser tabs, MCP/LSP connections, memory handles, and
other state. Never accept a model-supplied owner in place of it. The instance
ID changes across a complete process-host rebuild even when generation numbers
restart; the generation changes on extension reload or automatic restart
within one host instance. An old handle must not be used when either fence
changes. Other contribution contexts do not currently carry this field and
should not allocate session-owned handles.

Request typed, ephemeral frontend input from inside a handler:

```python
password = ext.request_input("Password:", secret=True)
if password is None:
    return tool_result(text_content("Input was cancelled."), is_error=True)
```

The wire is `input/request` with `{parent_request_id, prompt, secret}` and the
response is `{value: string|null}`. Prompts must contain non-whitespace text and
are bounded to 16 KiB UTF-8; values are bounded to 256 KiB UTF-8; the full
protocol-message bound also applies. `None` means cancellation or
unavailable/headless input. Secret answers stay on the private reply channel
and the SDK never logs them or places them in progress, metadata, or results.
Python strings cannot be reliably wiped in place, so keep a returned secret
short-lived and never copy, log, persist, or return it.

Subscribe to observational lifecycle events with slash or underscore names:

```python
@ext.on_lifecycle("turn_settled")
def turn_settled(event):
    ext.log.info(
        "turn settled",
        turn_id=event.get("turn_id"),
        outcome=event.get("outcome"),
    )
```

The supported notification methods are `session/started`, `session/settled`,
`turn/started`, `turn/settled`, `tool/started`, and `tool/settled`. Registered
subscriptions are returned in `protocol.lifecycle_events` during initialize.
When no lifecycle handlers are registered, the SDK does not negotiate the
feature (an empty negotiated subscription means "all" at the host). Lifecycle
handlers are observational and cannot veto the host transition.

The [`caffeinate`](../../examples/extensions/caffeinate) example is the current
API `0.2`, version `0.2.0` lifecycle proof. It reference-counts owning/root
`turn/started` and `turn/settled` events, clears remaining state on
`session/settled`, and explicitly stops its bounded macOS helper during
extension shutdown. Sleep inhibition is entirely extension-owned; no core Ygg
inhibitor remains.

For a host-managed capability, submit a structured action intent before the
side effect. When the optional `approvals` feature is available, an approved
`ask` carries a one-use token and the extension retries the exact intent:

```python
intent = {
    "kind": "external_side_effect",
    "operation": "browser.submit_form",
    "target": {"origin": "https://example.com", "label": "Publish comment"},
    "data_classes": ["user_text"],
    "adapter_hints": {"read_only": False, "destructive": False},
}
policy = ext.evaluate_policy(intent)
if policy["decision"] == "ask" and policy.get("approval_token"):
    policy = ext.evaluate_policy(
        intent,
        approval_token=policy["approval_token"],
    )
if policy["decision"] != "allow":
    return tool_result(text_content("The host denied the action."), is_error=True)
```

`evaluate_policy` requires the negotiated `policy_intents` feature and carries
the ambient parent request ID. Adapter hints are non-authoritative; only the
host returns `allow`, `ask`, or `deny`. `approval_token=` requires negotiated
`approvals` and exactly 64 lowercase hexadecimal characters. The SDK accepts a
returned token only with an `ask` decision. The host binds it to the canonical
original intent, still-active owner/parent, process generation, and short
expiry, then consumes it atomically on retry; mismatch, expiry, or reuse denies.
The coding product currently offers no approvals and its policy supervisor
returns `deny`.

Resolve a brokered secret by exact manifest name:

```toml
[capabilities]
secrets = ["browser.api_token"]
```

```python
api_token = ext.get_secret("browser.api_token")
```

`get_secret(name, *, parent_request_id=...)` requires negotiated `secrets` and
the same active owner-scoped parent correlation as other host services. The
manifest list is an exact allowlist, not environment injection: names are
duplicate-free, at most 64 ASCII bytes, start with a letter or underscore, and
then use only letters, digits, underscore, hyphen, or dot. Ygg supplies the
broker with the manifest-bound extension identity, the full resource-owner
triple, parent request ID, and requested name. A no-value result and provider
failure both surface as the same `-32004` `secret is unavailable` error.

Secret values are UTF-8 strings capped at 64 KiB, a bound the SDK validates as
well. The host does not persist or log them and best-effort wipes its broker and
writer buffers. Python receives an ordinary immutable string, so end-to-end
zeroization is not possible: keep it short-lived and never include it in logs,
progress, results, metadata, or storage. The coding product currently configures
no secret broker and therefore does not offer `secrets`.

Extensions can send protocol-safe user notifications and correlated host
requests without touching stdout directly:

```python
ext.notify("Ready", level="success", title="local workflow")
if ext.confirm("Continue?", destructive=True):
    ...
```

For frozen API `0.1`, the SDK uses unsigned numeric IDs for its own confirmation
requests. API `0.2` uses short `py:`-prefixed string IDs so bidirectional
cancellation cannot confuse a child request with the same numeric host request
ID. Raw protocol clients may use either an unsigned number or a string ID of at
most 256 UTF-8 bytes.

`stdout` is reserved for JSON-RPC. Use `ext.log.info(...)` (or the other log
levels) for structured JSON diagnostics on stderr. For graceful shutdown, the
host sends `shutdown`; the SDK stops admitting work, lets admitted handlers
drain up to the bounded deadline, requests cooperative cancellation for the
remainder, runs an optional `@ext.on_shutdown` handler, acknowledges shutdown,
and exits. Stdin EOF uses the same bounded drain path before treating the
transport as lost.
