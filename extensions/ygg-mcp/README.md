# ygg-mcp

`ygg-mcp` is Ygg's first-party API `0.2` bridge for explicitly configured
[Model Context Protocol](https://modelcontextprotocol.io/) tool servers. One
resident extension process owns every configured server session and maps its live
tool catalog to Ygg's transactional `tools/register` and `tools/unregister` API.

```text
Ygg <- API 0.2 JSON-RPC -> ygg-mcp <- MCP JSON-RPC stdio -> local servers
                                 \-> MCP Streamable HTTP -> explicit remote endpoint
```

V1 preserves local stdio servers. Streamable HTTP code exists only as a blocked-
by-default **experimental** transport: it requires a conspicuous one-shot CLI
opt-in from the Ygg process owner and is not suitable for production or sensitive
credentials. It does not support legacy MCP SSE endpoints, OAuth or browser
authorization flows, resources, prompts, sampling, elicitation, automatic server
installation, or ambient MCP discovery.

## Security and authority

Installing or discovering this package is inert. The bridge starts only after
Ygg independently admits the executable extension (enablement, exact trust, and
full-access startup policy), and it starts no MCP server unless an explicit
configuration file exists.

A local MCP server is arbitrary software running with the current user's OS
authority. Neither the bridge, its manifest, nor per-call approval is an OS
sandbox. Review and install every server separately; this bundle never copies,
downloads, or installs server software.

There are two separate decisions:

1. **Server/endpoint trust:** the user configuration, or a user-file digest pin
   for a trusted project configuration, names either exact direct launch
   arguments or one exact remote endpoint.
2. **Tool-call policy:** server trust does not approve every tool. Only an MCP
   annotation whose `readOnlyHint` value is exactly JSON `true` (and which is
   not contradicted by positive destructive/open-world hints) receives the
   read-only classification. Missing, false, numeric, string, or malformed
   annotations are `unknown`. Destructive/open-world hints increase caution.

An explicitly read-only tool may run without an additional prompt. Every
`unknown` or `destructive` call goes through the negotiated host
`policy/evaluate` service. If policy intents are unavailable, evaluation fails,
or the host denies the intent, the bridge fails closed. It uses a one-use
approval retry only when the host actually negotiates `approvals`; Ygg `0.6.7`'s
coding product does not currently enable approval issuance, so those calls are
denied with an explanatory tool error. An MCP tool call is never automatically
replayed after timeout, cancellation, crash, or an ambiguous disconnect.

Server descriptions, schemas, logs, errors, and results are untrusted data.
Descriptions and schema text are bounded and explicitly labeled untrusted;
they cannot select lifecycle actions or lower policy. Compact presentation
never contains the server command, arguments, environment, credentials, raw
server descriptions, or raw logs. MCP stderr is drained into a bounded,
credential-redacted in-memory ring and is not copied into frontend state.

The server environment begins from a small non-secret process allowlist
(`PATH`, locale, and temporary-directory variables) plus only the `env` entries
in the explicit stdio configuration. It does not inherit dotenv files or ambient
provider/application tokens. Explicit `env` values are sensitive configuration:
protect the file and never place secrets in labels or arguments.

### Experimental Streamable HTTP gate

Local stdio MCP remains available through normal reviewed configuration. Remote
Streamable HTTP MCP is denied unless the **Ygg process owner** supplies this
one-shot command-line switch for that process:

```console
ygg --experimental-streamable-http-mcp \
    --enable-extension ygg-mcp --trust-extension ygg-mcp
```

This is intentionally not a configuration feature. `~/.ygg/mcp.json`,
digest-pinned project MCP files, Ygg project/global configuration, environment
variables, session/host requests, and extension-manifest arguments cannot grant
it. The coding product strips a manifest-supplied copy and passes the switch to
the bridge only after parsing the product CLI. A disabled remote template stays
inert; enabling it without the switch fails during configuration loading, before
credential lookup, DNS, network I/O, or MCP manager workers. Lifecycle actions
also reject a disabled-gate remote before creating a worker.

For direct development/configuration validation, the same owner switch is
required on the bridge command:

```console
ygg-mcp --experimental-streamable-http-mcp --config ~/.ygg/mcp.json --check-config
```

A remote endpoint is a separate explicit network-trust decision. Streamable HTTP
uses the exact configured URL, TLS certificate/hostname validation, no proxy or
cookie discovery, and no redirects. HTTPS is required except for a numeric
loopback address, which exists for deterministic local development and tests.
URLs cannot contain userinfo, a query, or a fragment, preventing URL-auth and
query credential fields as well as endpoint switching by redirect. The extension
never synthesizes a browser `Origin` header or forwards browser credentials.

Remote `auth` contains only a logical credential reference, never a token or
header value. A host/application composition may inject the narrow
`CredentialProvider.bearer_token(reference, server_id=...)` adapter; the bridge
asks it at request time, uses the returned token only to form that request's
`Authorization: Bearer` header, redacts it from parsed remote data, then drops
it. The normal bundled runtime intentionally has no provider, so such a server
parks with `authentication_unavailable`. OAuth discovery, browser redirects,
token acquisition/refresh, persistent token stores, static config headers, and
secret environment fallback are not implemented.

### Known Streamable HTTP defects

The gate is a containment measure, not a claim that remote transport is safe.
Do **not** enable it for production, privileged networks, or sensitive
credentials. The known unresolved defects are:

1. HTTPS SSRF remains possible through DNS rebinding; connections are not pinned
   to a reviewed address.
2. Credential and session state can be shared across distinct resource owners.
3. DNS workers can outlive cancellation and shutdown (they are not reliably
   killable).
4. Buffered SSE handling can confuse peer identity.
5. Control-message fanout is unbounded.
6. Aggregate budgets can reset across remote transport paths.
7. Truncated framing can be accepted.
8. An empty SSE event ID can produce an incorrect resume cursor.
9. The startup deadline can be escaped.

These defects are deliberately left visible rather than hidden behind a config
knob. The only activation path is the process-owner opt-in above; remediation of
the defects, not broader configuration, is required before this transport can be
made generally available.

## Requirements and installation

- Ygg exactly `0.6.7` (`requires_ygg = "=0.6.7"`)
- Python 3.9 or newer on `PATH`
- separately installed MCP server executables

The release bundle includes the tested dependency-free Python extension SDK
under `vendor/`; startup never runs `pip`, a browser download, or install code.
With the first-party bundle installer, installation remains separate from
activation:

```console
ygg extension install ygg-mcp
ygg --enable-extension ygg-mcp --trust-extension ygg-mcp
```

A checkout can be tested explicitly without installation:

```console
ygg --extension-dir ./extensions \
    --enable-extension ygg-mcp \
    --trust-extension ygg-mcp
```

Executable extensions run only under Ygg's documented full-access process gate.
`--safe-mode` retains discovery but does not start this bridge.

## Configuration

The normal entrypoint reads `~/.ygg/mcp.json`. If that file is absent, the
bridge remains healthy with zero configured servers. Copy the disabled example
and edit it deliberately:

```console
mkdir -p ~/.ygg
cp extensions/ygg-mcp/config.example.json ~/.ygg/mcp.json
chmod 600 ~/.ygg/mcp.json
$EDITOR ~/.ygg/mcp.json
extensions/ygg-mcp/ygg-mcp --config ~/.ygg/mcp.json --check-config
```

If the file enables a Streamable HTTP server, add
`--experimental-streamable-http-mcp` to that validation command and to the Ygg
process that will own the bridge. The JSON has no equivalent setting.

`config.schema.json` is the normative JSON schema. The parser also rejects
unknown/duplicate keys, non-UTF-8 or oversized files, symlink final files,
linked `.ygg` roots or escaping trusted-project ancestors, files writable by
another user, files with explicit `env` values accessible by group/other users,
duplicate server IDs, NUL/control characters, mutually incompatible transport
fields, unsafe remote URLs, and values outside package ceilings. Commands are
direct argument arrays and never pass through a shell. Remote endpoints are exact
URL strings rather than discovery patterns; there is no raw `headers`, token,
password, or OAuth configuration field.

A minimal user file is:

```json
{
  "version": 1,
  "servers": {
    "local-example": {
      "transport": "stdio",
      "label": "Local example",
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "cwd": "/absolute/trusted/working-directory",
      "env": {},
      "enabled": true,
      "required": false
    }
  }
}
```

Server IDs are stable lowercase identifiers matching
`[a-z][a-z0-9-]{0,31}`. Labels are trusted user text; the MCP server's own name
is never used as a UI label. Relative `cwd` values resolve from the file that
defines the server. A missing executable or invalid MCP handshake is a
permanent failure parked until an explicit restart/config refresh.

### Streamable HTTP configuration

This experimental configuration is inert without the process-owner
`--experimental-streamable-http-mcp` flag. Adding the flag to JSON, a trusted
project file, an environment variable, or a session does not work.

A remote server must opt into `"transport": "streamable-http"` and name one
absolute endpoint. It accepts `https`; `http` is accepted only for literal
`127.0.0.1` or `::1` loopback endpoints. Stdio launch fields (`command`, `args`,
`cwd`, and `env`) are rejected for this transport.

```json
{
  "version": 1,
  "servers": {
    "remote-example": {
      "transport": "streamable-http",
      "label": "Reviewed remote MCP",
      "url": "https://mcp.example.invalid/mcp",
      "enabled": true,
      "required": false,
      "auth": {
        "type": "bearer",
        "credential": "reviewed_remote_mcp"
      }
    }
  }
}
```

`credential` is a bounded logical reference, not a secret. It requires an
application-provided `CredentialProvider`; the stock executable fails closed
without one. Omit `auth` for an endpoint that does not need authorization. Do
not put tokens in a URL, label, argument, `env`, or any other config field. The
parser rejects remote header/auth-value fields and this package deliberately does
not offer static HTTP headers.

### Digest-pinned trusted project configuration

Project files are not discovered or launched on their own. The user file must
name an **absolute** file beneath the active workspace's `.ygg/` directory and
pin its exact bytes:

```json
{
  "version": 1,
  "servers": {},
  "trustedProjects": [
    {
      "path": "/absolute/workspace/.ygg/mcp.json",
      "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }
  ]
}
```

Generate the digest after review (for example, `shasum -a 256 FILE`). Any edit
invalidates the pin and leaves the bridge in an inspectable degraded state; it
does not execute the changed project command. Project files may contain only
`version` and `servers`, cannot include another file, and cannot override a user
server ID.

### Enforced default bounds

| Resource | Default | Package maximum |
| --- | ---: | ---: |
| configured servers | 16 | 32 |
| tools per server | 64 | 128 |
| total published tools | 256 | 256 |
| catalog pages | 8 | 32 |
| MCP frame/result | 8 MiB | 16 MiB |
| concurrent calls | 8 | 32 |
| pending requests per server | 16 | 64 |
| retained stderr entries | 128 | 1,024 |
| retained stderr line | 4 KiB | 16 KiB |
| startup timeout | 5 s | 30 s |
| request timeout | 30 s | 120 s |
| shutdown stage | 1.5 s | 5 s |
| automatic restart attempts | 5 | 8 |
| retry backoff cap | 30 s | 60 s |

Catalog pagination is cycle-checked. Tool/schema text, structured output,
content parts, text, individual/aggregate media, presentation nodes,
activities, and action counts have additional fixed bounds in source.

### Streamable HTTP framing and recovery

The HTTP client POSTs one JSON-RPC message with `Content-Type: application/json`
and `Accept: application/json, text/event-stream`. It accepts a bounded JSON
response or a bounded SSE response; `202 Accepted` is accepted only for
notifications. After a valid `initialize` response, a validated in-memory
`Mcp-Session-Id` is sent on subsequent requests with the negotiated
`MCP-Protocol-Version`. A changed or malformed session identity fails closed.
A `404` for an established session is treated as expiration and triggers the
normal fresh-session reconnect path.

SSE response events are UTF-8 JSON-RPC `message` events, bounded by the existing
frame limit both per event and in aggregate (at most 256 events). A server-issued
SSE `id` is retained only in memory. If a POST response stream closes before its
terminal response *after* such an ID, the bridge may perform at most the
configured `maxRestarts` bounded GET resumptions with `Last-Event-ID`; it never
re-POSTs the original request. Without an ID, an interrupted request is
ambiguous and is not replayed. Server-provided SSE `retry` values are capped by
the configured backoff maximum. This path does not open a permanent optional GET
notification stream and does not implement the retired standalone/legacy SSE
transport.

HTTP response bodies, event streams, request slots, timeouts, and shutdown are
bounded by the existing limits. Redirects are rejected before following a
`Location`; 401/403 park with a generic authentication error; malformed,
oversized, unsupported-content-type, and unsafe status responses park without
retaining response text. Rate limits and transient transport/server failures use
the existing bounded lifecycle backoff (honouring a capped numeric `Retry-After`
when present). Cancellation aborts the in-flight socket and sends one bounded
`notifications/cancelled`; it never claims rollback or replays the request.

## Catalogs, calls, and results

The initialize catalog is empty epoch `0`; configured servers start only after
the Ygg initialize response has been flushed. Each successful server catalog
change publishes complete dynamic definitions. The bundled API `0.2` SDK keeps
eight committed Ygg schema/handler snapshots, so an older in-flight model turn
uses the handler and validation schema from the `catalog_revision` it saw.
Removed/restarted servers never alias an old epoch to a new connection.

Calls use bounded concurrency and timeout, forward Ygg cancellation as MCP
`notifications/cancelled`, and retain safe server/tool provenance and terminal
activity. Cancellation requests cooperation and never claims rollback.
Server-reported progress is reduced to bounded numeric progress; untrusted
progress messages are not promoted to UI authority.

MCP results cross the normal API `0.2` boundary:

- text remains ordered model-visible text;
- an MCP `structuredContent` paired with `outputSchema` becomes validated Ygg
  `structured_content`;
- schema-less structured content is retained in bounded, non-model-visible
  metadata because API `0.2` forbids `structured_content` without a declaration;
- supported image/audio base64 is written to the generation scratch directory,
  published through `artifact/publish`, then removed locally; and
- malformed, unsupported, or oversized content returns a bounded tool error.

Supported media matches the host artifact verifier: PNG, JPEG, GIF, WebP, WAV,
MPEG audio, FLAC, Opus, AAC, and MP4 audio. Artifact IDs remain bound to the
active host-derived session owner and process generation.

## Lifecycle, health, and recovery

Servers transition through configured, connecting, ready, refreshing,
degraded, backoff, parked, and stopped states. Transient crashes reconnect with
bounded full-jitter exponential backoff. Permanent configuration/protocol
failures and exhausted retry budgets park. Refresh reads a catalog without
relaunching; restart explicitly replaces a connection; stop removes its current
tools and closes it. Shutdown closes all roots in bounded parallel workers, and
Ygg's extension process-group cleanup is the final descendant fence.

Use the narrow/headless fallback in every frontend:

```text
/mcp status
/mcp list
/mcp snapshot
/mcp show <server>
/mcp refresh [server]
/mcp restart <server>
/mcp stop <server>
```

`/mcp snapshot` returns the same generic semantic snapshot published through API
`0.2` `presentation/update`. Lifecycle actions route only to the manifest-
declared `mcp` command with literal bridge-authored arguments; server text and
model output cannot manufacture an action.

## TUI and Serve presentation

The package emits only Ygg's generic semantic presentation contract—compact
status, bounded activity, a host-rendered server/tool tree, selected detail, and
declared actions. It ships no ANSI renderer, terminal widget, web JavaScript, or
MCP manager in core. TUI and Serve remain projections of the resident bridge.

The compact status is like `mcp 2/3 · 7 tools · degraded`. Server nodes expose
safe lifecycle/transport/catalog/restart metadata; tool nodes expose only their
sanitized Ygg name, schema counts, and approval classification. Complete
snapshots make reconnect/resync side-effect-free: reading state never launches a
server, refreshes a catalog, repeats a call, or revives a retired epoch. The host
process generation fences stale removal after bridge reload.

`fixtures/presentation/` contains deterministic generic snapshots for empty,
connecting/loading, ready, refreshing, degraded, parked, restarted, running,
succeeded, failed, cancelled, and ambiguous states, plus official Serve
projection fixtures for reconnect/resync and stale-generation removal. The same
fixtures are frontend-neutral and are intended for both TUI and Serve reducers.

## Tests

From this source root:

```console
python3 -m unittest discover -s tests -t . -v
```

The dependency-free suite covers strict config/trust, real and adversarial stdio
servers, deterministic loopback Streamable HTTP framing/session/auth/SSE fixtures,
add/replace/remove catalogs, epoch-pinned schemas, malformed/oversized frames,
cancellation, timeout, crash/restart/parking, bounded redacted logs, media
artifacts, policy failure, shutdown, API `0.2` wire behavior, generic
presentation fixtures, and release/package smoke checks.
