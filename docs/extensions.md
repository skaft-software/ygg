# Executable extensions

> **Protocol reference:** [`docs/extensions/PROTOCOL-REFERENCE.md`](extensions/PROTOCOL-REFERENCE.md)
> contains the complete specification of every JSON-RPC method, request/response
> shape, type reference, and lifecycle timing.

Ygg supports trusted local extension processes alongside native Rust
`Extension` implementations. Process extensions use JSON-RPC 2.0 messages,
one compact JSON object per line, over stdin/stdout. They may be written in any
language that can read and write JSON lines.

Executable extensions are intentionally a local tinkerer feature. Capability
declarations are visible consent metadata, not an operating-system sandbox.
An enabled extension runs as the current user and must also receive a separate
trust grant before Ygg launches it.

## Layout and discovery

Each direct child directory contains one file named `extension.toml`:

```text
.ygg/extensions/git-tools/extension.toml
~/.ygg/extensions/git-tools/extension.toml
```

Precedence is global, then trusted project, then explicit directories in
command-line order; later definitions win by extension directory name. Project
extensions are ignored until the workspace is trusted. Discovery alone never
executes code: enablement and executable trust are independent, explicit
decisions bound to the selected manifest name and source.

The direct child directory name must exactly match `name` in its manifest.
This makes the shared resolver's later-wins precedence authoritative for both
discovery and trust; aliases are rejected with a diagnostic.

Use repeatable command-line options for one-off tinkering:

```console
ygg --extension-dir ./my-extensions \
    --enable-extension hello-world \
    --trust-extension hello-world
```

Or persist activation in the user config:

```toml
enabled_extensions = ["hello-world"]
trusted_extensions = ["hello-world"]
```

A bare persistent trust name applies only to the matching extension under
`~/.ygg/extensions`. It never transfers to a same-named project or explicit
extension. Persist trust for either of those sources with its exact absolute
manifest path:

```toml
enabled_extensions = ["git-tools"]
trusted_extensions = [
  "git-tools@/absolute/project/.ygg/extensions/git-tools/extension.toml",
]
```

`--trust-extension git-tools` is deliberately different: it trusts the
currently selected `git-tools` source for this process invocation only and is
never written back as a persistent name grant.

A trusted project config may suggest `enabled_extensions`, but it cannot grant
itself executable trust. Persistent trust must come from the user config or
environment (`YGG_TRUSTED_EXTENSIONS`); one-shot trust comes from
`--trust-extension`.

The agent crate exposes both pieces of the boundary:

- `discover_extension_manifests` scans conventional direct-child layouts.
- `ExtensionCatalog::load_resolved` accepts already resolved manifest paths in
  authoritative precedence order, retaining diagnostics instead of making one
  bad extension disable the catalog.

Manifest reads are bounded. Selected files must be regular, non-symlink files;
malformed or shadowed resources produce inspectable diagnostics without
preventing the core binary from starting.

## Manifest

```toml
name = "git-tools"
version = "0.1.0"
api_version = "0.1"
description = "Small local git helpers"

[entrypoint]
command = "git-tools"
args = ["--stdio"]

[capabilities]
filesystem = "workspace" # none, workspace, or unrestricted
process = true
network = false

[contributes]
tools = ["git_status"]
commands = ["checkpoint"]
hooks = ["after_tool_call"]
ui = ["status"] # status, header, or footer
context = true
tool_renderers = ["git_status"]
notifications = true
confirmations = true
```

Bare entrypoint commands are first resolved beside the manifest, then through
`PATH`. Arguments are passed directly without a shell. The child working
directory is the active workspace. Ygg supplies `YGG_EXTENSION_API_VERSION`,
`YGG_EXTENSION_NAME`, `YGG_EXTENSION_DIR`, `YGG_EXTENSION_MANIFEST`, and
`YGG_WORKSPACE`.

## Transport contract

Stdout is protocol-only. Human diagnostics belong on stderr, which Ygg drains
and exposes as bounded diagnostic events. The default maximum JSON line is 1
MiB, the default in-flight request cap is 64, and ordinary requests time out
after 30 seconds.

Every request and response uses the standard JSON-RPC envelope:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":1,"result":{}}
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid params"}}
```

The initial host request is always `initialize`. Its parameters include API and
Ygg versions, extension identity and source, the workspace, capability and
contribution declarations, and inspectable session/model/reasoning/active-skill
state. The response must use the same API version and provide complete schemas
for exactly the tools and commands declared in the manifest:

```json
{
  "api_version": "0.1",
  "tools": [{
    "name": "git_status",
    "description": "Show compact workspace status",
    "parameters": {"type":"object","properties":{}}
  }],
  "commands": [{
    "name": "checkpoint",
    "description": "Record a local checkpoint",
    "usage": "/checkpoint [label]"
  }]
}
```

Host-to-extension methods are typed in
`ygg_agent::extension_process::methods`:

| Method | Result |
| --- | --- |
| `tool/call` | `{content, is_error, metadata}` |
| `command/execute` | `{text, notifications, context}` |
| `hook/run` | `{disposition, context, notifications}` |
| `context/collect` | array of `{label, content, placement}` |
| `status/collect` | a semantic status contribution or `null` |
| `tool/render` | ordered `{segments: [{text, style_role}]}` |
| `shutdown` | any JSON result, followed by process exit |

Extensions may send these process-to-host messages at any time after
initialization:

| Method | Envelope |
| --- | --- |
| `notification` | notification; no `id` |
| `confirmation/request` | request with string or numeric `id`; Ygg answers that `id` |
| `context/contribution` | unsolicited context notification |
| `status/contribution` | unsolicited semantic TUI notification |

All TUI contributions contain plain text and optional semantic style roles.
Raw terminal escape sequences are not part of the extension API.
Tool-renderer segments are accepted and retained as internal extension
provenance, but are never rendered in the TUI or exposed through Ctrl+O,
`/verbose`, transcript selection, or copy. The original tool result
remains immutable evidence for the agent's required protocol result,
session persistence, and export redaction policy; it is not a presentation
surface. Extension header, status, footer, notification, and confirmation
features remain separate Ygg UI surfaces.

In the interactive frontend, confirmation requests made while an extension
tool or command is running open a typed allow/deny panel. Dropping the request
or using a non-interactive frontend denies it. Requests that arrive outside an
active confirmation boundary are also denied; they are never implicitly
accepted.

Use `/extensions` to inspect discovered, enabled, trusted, and running state.
Each entry includes the selected manifest path. An enabled-but-untrusted entry
reports the exact copyable persistent grant as well as the one-shot CLI form.
Use `/extensions reload` to replace each running process after a successful
handshake. The general `/reload` command re-runs resource discovery and rebuilds
the product boundary.

## Python SDK

Ygg ships a dependency-free Python package for extensions that use the stdio
protocol. Install it from a checkout before copying an example or building an
extension:

```console
python3 -m pip install ./sdk/python
```

The package is named `ygg-extension-sdk` and exposes `ygg_extension.Extension`.
Decorate tools and commands with their manifest metadata, then call
`Extension.run()`; the SDK owns JSON-RPC 2.0 JSON-lines framing, flushes every
reply, validates the initialize negotiation, dispatches all typed host methods,
keeps structured logs on stderr, and exits on shutdown or stdin close.

```python
from ygg_extension import Extension

ext = Extension()

@ext.tool(name="hello_world", description="Greet someone")
def hello(args):
    return {"content": "Hello!"}

ext.run()
```

Tool and command names must exactly match the manifest. The SDK also provides
decorators for hooks, prompt context, status surfaces, and tool renderers, plus
`notify()` and correlated `confirm()` helpers for process-to-host messages.
See [`sdk/python/README.md`](../sdk/python/README.md) for the complete API and
the three runnable examples below.

## Lifecycle and reload

`ExtensionProcess` implements the existing native `Extension` trait, so its
negotiated tools register through `ExtensionHost` and retain the agent's normal
duplicate detection and non-replayable safety default. Product frontends call
the typed command, hook, context, status, renderer, notification, and
confirmation APIs at their corresponding semantic boundaries.

Reload starts and fully initializes a replacement before swapping it in. The
existing process stays active if launch or handshake fails. Because an
`ExtensionHost` has already registered tool objects, a reload that changes tool
or command schemas is rejected with a clear "re-registration required" error;
the frontend can then rebuild its host intentionally. Pending confirmation IDs
carry a process generation and cannot be answered against a replacement child.

Shutdown is requested gracefully and bounded by a short timeout. A process
that does not exit is killed, and dropping the last runtime handle also uses
kill-on-drop cleanup.

The SDK-backed examples remain small and copyable:

- [`hello-world`](../examples/extensions/hello-world) demonstrates the minimum
  process and protocol handshake.
- [`git-tools`](../examples/extensions/git-tools) contributes a bounded custom
  tool, command, and semantic renderer.
- [`local-model-workflow`](../examples/extensions/local-model-workflow)
  demonstrates prompt hooks, deterministic context, status, and notifications.

## First-party application packages

Application packages are separate from the executable-extension protocol above.
They distribute a complete first-party application runtime rather than JSON-RPC
tools or hooks, use `package.toml` instead of `extension.toml`, and are never
loaded during ordinary agent startup. The `0.3.2-alpha` package manager supports
only the official `ygg-serve` package and local copies of that release archive.
It is intentionally not a general package registry.

```console
ygg extension install ygg-serve
ygg extension list
ygg extension update ygg-serve
ygg extension remove ygg-serve
ygg serve
```

Packages are installed under `~/.ygg/extensions/ygg-serve/`:

```text
package.toml
bin/ygg-serve-runtime
install.json
```

The manifest is schema-versioned and declares the package ID and version, an
exact required Ygg version, target triple, entrypoint arguments and SHA-256,
and loopback/process/workspace capabilities. Official installs download the
matching release archive and `SHA256SUMS` over HTTPS. Local archives use:

```console
ygg extension install --path ./ygg-serve-0.3.2-alpha-TARGET.tar.gz
```

Installation validates the bounded archive and embedded executable checksum,
rejects links and unexpected paths, and publishes the package with an atomic
same-filesystem rename. `ygg serve` revalidates compatibility and the executable
checksum before replacing the launcher process with the installed runtime.
Removal deletes only package files; sessions, project metadata, and other user
data remain outside the package directory.
