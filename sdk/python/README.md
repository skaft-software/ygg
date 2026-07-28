# ygg-extension-sdk

`ygg-extension-sdk` is the dependency-free Python SDK for Ygg's executable
extension protocol. It owns JSON-RPC 2.0 JSON-lines framing, flushes every
response, validates the `initialize` negotiation against the selected manifest,
and keeps diagnostics on structured stderr logs.

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

The tool name must also appear in the extension manifest:

```toml
[contributes]
tools = ["hello_world"]
```

The host sends the manifest, workspace, and current session/model state in the
first `initialize` request. Tool and command decorators must exactly match the
manifest declarations; a mismatch is rejected during the handshake instead of
being silently advertised.

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

Extensions can send protocol-safe user notifications and correlated host
requests without touching stdout directly:

```python
ext.notify("Ready", level="success", title="local workflow")
if ext.confirm("Continue?", destructive=True):
    ...
```

`stdout` is reserved for JSON-RPC. Use `ext.log.info(...)` (or the other log
levels) for structured JSON diagnostics on stderr. When the host sends
`shutdown`, the SDK acknowledges it and exits; closing stdin also ends the loop
without requiring extension-specific cleanup code.
