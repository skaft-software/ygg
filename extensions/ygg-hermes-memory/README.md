# ygg-hermes-memory

`ygg-hermes-memory` is Ygg's first-party API `0.2` compatibility bridge for an
**already installed, explicitly selected** provider implementing Hermes Agent's
`MemoryProvider` contract. It keeps memory outside the Ygg kernel and outside
Ygg sessions:

```text
Ygg <- API 0.2 JSON-RPC -> ygg-hermes-memory -> one selected Hermes provider
                                                    |
                                                    +-- provider-owned store
```

The bridge targets this exact upstream contract, not whichever `hermes` command
happens to be on `PATH`:

- Hermes Agent package `0.20.1`
- commit `7095e23eb2066fe9a2f93b99cdbfe0e2b5ece397`
- `agent/memory_provider.py`
- `hermes_agent.memory_providers` entry-point group

Providers, Python environments, dependencies, credentials, models, indexes,
embeddings, and stores are **not included**. The package has no install hook and
never runs `pip`, downloads a model, provisions a database, or creates a
provider store.

## Authority and trust

Installing or discovering this bundle is inert. Three independent decisions are
required before provider code runs:

1. Ygg must enable and trust the executable extension under its normal
   full-access process gate.
2. The user must configure one exact provider environment and source.
3. The selected provider's current metadata/code fingerprint must be trusted.

Discovery reads bounded distribution metadata, `plugin.yaml`, and provider
Python bytes for a trust digest. It does **not** import a module, call
`register()`, construct a provider, call `is_available()`, or initialize a
backend. `/memory trust` still does not import. The first import occurs only at
`/memory select` (or an explicitly configured, fingerprint-trusted
`defaultProvider`).

A selected provider is arbitrary local code running with the current user's OS
authority. The manifest declares unrestricted filesystem, process, and network
capabilities because compatible providers may own local or remote backends.
These declarations and the picker are consent metadata, not an OS sandbox. Use
Ygg full-access mode only inside the isolation boundary you consider adequate.
`--safe-mode` leaves discovery visible but does not start the extension.

Exactly one provider instance is active per host-derived Ygg resource owner.
Instances, prompt caches, queues, activities, and calls are keyed by the complete
`{session_id, extension_instance_id, process_generation}` fence when API `0.2`
supplies it. A stale generation is retired and never aliases a new owner. A
provider never receives Ygg's session registry, a session path, or unrestricted
transcript files.

## Requirements and installation

- Ygg exactly `0.6.0-dev` (`requires_ygg = "=0.6.0-dev"`)
- an already provisioned Hermes Agent `0.20.1` environment (upstream currently
  requires Python 3.11 through 3.13)
- any provider dependencies installed by the user in that environment

Install and activation remain separate:

```console
ygg extension install ygg-hermes-memory
ygg --enable-extension ygg-hermes-memory --trust-extension ygg-hermes-memory
```

From a checkout:

```console
ygg --extension-dir ./extensions \
    --enable-extension ygg-hermes-memory \
    --trust-extension ygg-hermes-memory
```

The bundle vendors only Ygg's dependency-free Python extension SDK under
`vendor/`. `extension.py` reads the configured interpreter locator and `exec`s
that already existing Python before importing Hermes or a provider. This keeps
one long-lived Python process rather than spawning a second provider daemon.

## Provider environment setup

Provision Hermes and the provider yourself. For example (commands vary by
provider and are never run by Ygg):

```console
python3.13 -m venv ~/.venvs/hermes-memory
~/.venvs/hermes-memory/bin/python -m pip install 'hermes-agent==0.20.1'
# Install the chosen provider according to its upstream documentation.
```

Copy `config.example.json` to `~/.ygg/hermes-memory.json`, replace every absolute
placeholder, and protect it:

```console
mkdir -p ~/.ygg
cp extensions/ygg-hermes-memory/config.example.json ~/.ygg/hermes-memory.json
chmod 600 ~/.ygg/hermes-memory.json
$EDITOR ~/.ygg/hermes-memory.json
```

The required environment block is:

```json
{
  "id": "hermes-0.20.1-local",
  "python": "/home/me/.venvs/hermes-memory/bin/python",
  "hermesHome": "/home/me/.hermes",
  "providerEnvFile": "/home/me/.hermes/.env",
  "includeEntryPoints": true
}
```

`id` is the safe identity shown in presentation. Interpreter and backend paths
are never placed in presentation, activities, tool metadata, or diagnostics.
The configured interpreter path must exactly be the running `sys.executable`
(so two virtual environments sharing a base binary do not alias), its installed
`hermes-agent` distribution metadata must be exactly `0.20.1`, and the imported
`agent.memory_provider` module must resolve to that distribution's recorded
contract file rather than a workspace or `PATH` shadow.

### Credentials

The coding product does not currently configure Ygg's secret broker. Do not put
credential values in `extension.toml`, this JSON file, commands, prompts,
sessions, or artifacts.

When a provider requires environment credentials, `providerEnvFile` may name an
explicit provider-owned dotenv-like file. It is read **only after provider trust
and selection**, must be a current-user-owned regular non-symlink file with mode
`0600`, and is bounded to 64 KiB/128 names/16 KiB per value. Values are loaded
without shell expansion, never logged or presented, and removed from the
extension environment on shutdown. Dynamic-loader, Python, Ygg, `PATH`, and
home-directory controls are rejected. Set the field to `null` or omit it for a
provider using only native files under `hermesHome`.

Python strings cannot promise zeroization. The provider receives ordinary
process environment strings, just as it does under Hermes. Keep the file scoped
to this provider environment and rely on OS isolation for stronger protection.

## Discovery sources

### Directory providers

Directory sources are exact paths; the bridge never scans the workspace or an
ambient project tree:

```json
{
  "id": "my-provider",
  "path": "/home/me/.hermes/plugins/my-provider",
  "label": "My provider",
  "network": "required",
  "storage": "remote",
  "setup": "configured",
  "readTools": ["recall_my_provider"],
  "writeTools": ["remember_my_provider"]
}
```

The directory must be a non-symlink package with `__init__.py`. Its Python files
and `plugin.yaml` are hashed under fixed file/count/byte ceilings. Symlinks and
special files fail closed. The module can expose `register(ctx)`, a
`MemoryProvider` instance/class, or one unambiguous subclass, matching the pinned
Hermes loader shapes. Secondary skill/CLI/general plugin registrations are
ignored: this bridge owns only the `MemoryProvider` compatibility surface.

### Installed entry points

With `includeEntryPoints: true`, the configured environment enumerates the
`hermes_agent.memory_providers` group through `importlib.metadata`. The digest
binds the entry-point name/value, distribution name/version, bounded installed
Python/native code files, environment ID, and Hermes contract. Distribution
modules remain unimported until selection. Configured directories retain
Hermes's earlier-source precedence on a provider name collision.

`providerMetadata` supplies safe behavior declarations for an entry point:

```json
{
  "entrypoint:my-provider": {
    "label": "My provider",
    "network": "required",
    "storage": "remote",
    "setup": "required",
    "readTools": [],
    "writeTools": ["remember"]
  }
}
```

These declarations inform the picker; they do not lower extension authority or
prove provider behavior.

## Fingerprints, selection, and setup state

Validate and enumerate metadata without importing providers:

```console
extensions/ygg-hermes-memory/ygg-hermes-memory \
  --config ~/.ygg/hermes-memory.json --check-config
extensions/ygg-hermes-memory/ygg-hermes-memory \
  --config ~/.ygg/hermes-memory.json --discover
```

Review the provider/environment and provider source, then either:

- add the exact digest under `trustedProviders` and optionally set
  `defaultProvider`; or
- use `/memory trust ID FINGERPRINT`, which trusts only this extension process,
  followed by `/memory select ID`.

Changing directory code or entry-point identity invalidates trust. A trusted
`defaultProvider` begins bounded activation asynchronously at the first fully
owner-scoped prompt/context boundary, never during metadata discovery or under
the initialize request's provisional display session. The admitting prompt does
not wait for a slow provider; it proceeds without memory while status says
loading, and later prompt epochs use the provider after it is ready. Use an
explicit idle `/memory select ID` before the turn when deterministic first-turn
availability is required. Selection and switching are idle-boundary operations.
Switching first fences queued work and shuts down the old instance; a failed
replacement cannot leave stale dynamic tools callable. Unselected code is never
imported or initialized.

## API `0.2` mapping

| Hermes contract | Ygg mapping |
|---|---|
| `get_tool_schemas()` | bounded validated `tools/register` dynamic catalog |
| `handle_tool_call()` | catalog-epoch-pinned `tool/call` handler |
| `system_prompt_block()` | activation-frozen, fenced `context/collect` system suffix |
| `prefetch()` / `recall_status()` | prompt-epoch-frozen, bounded `context/collect` prompt prefix + content-free read provenance |
| `on_turn_start()` | `before_prompt` with bounded/redacted user text |
| `sync_turn()` | captured `before_prompt` user + successful API `0.2` `after_response` assistant text |
| `queue_prefetch()` | completed `turn/settled` background task |
| `on_memory_write()` | committed, non-staged built-in `memory` `after_tool_call` only |
| `on_session_end()` | `session/settled` with at most 32 bounded in-process user/assistant snippets |
| `shutdown()` | bounded session/extension shutdown and final host process-tree fence |

`turn/settled` remains authoritative for failure, cancellation, interruption,
frontend loss, and shutdown cleanup. Failed turns are never given invented
assistant content and are not synced. Ygg's dynamic catalog is process-scoped,
so the bridge publishes only the currently composing/selected owner's one
provider rather than merging providers. Epoch-pinned handlers re-check the full
call owner and selected provider; a concurrent owner cutover may reject a stale
call, but can never route it into another owner's provider.

The current generic API has no equivalent
pre-compression/delegation boundary, so `on_pre_compress()` and
`on_delegation()` are not invoked; `/memory lifecycle` and provider detail report
that honestly. `backup_paths`, provider setup writers, provider CLI commands,
and provider skills are also not exposed.

## Untrusted memory boundary

Memory and provider text are data, never instructions or authority:

- static and recalled text is control/ANSI-sanitized, credential-redacted,
  provider-marker-neutralized, line-prefixed, and enclosed in an explicit
  `YGG_UNTRUSTED_MEMORY` fence;
- context is capped in aggregate and frozen for the current prompt epoch, so a
  retry/reconnect cannot repeat a read or change an in-flight prompt;
- the activation-time system block is immutable until explicit reload, switch,
  or a new owner; a write cannot silently rewrite it;
- tool descriptions/schemas are bounded and restricted to Ygg's supported JSON
  Schema vocabulary;
- tool arguments/results are strict bounded JSON; sensitive result keys and
  credential-like strings are redacted; and
- provider exception text, backend paths, raw logs, memory text, credentials,
  embeddings, and indexes never enter status, provenance, or diagnostics.

A provider tool result remains model-visible because it is the requested tool
result, but it is fenced and labeled untrusted. `structured_content` is not
invented because Hermes's contract does not declare a Ygg output schema.

## Durability and lifecycle provenance

The extension emits bounded generic `presentation/update` snapshots only. It
ships no TUI widget, web code, ANSI renderer, or memory-specific host manager.
TUI and Serve render the same data:

- picker/list/detail nodes for `Off` and metadata-only providers;
- provider/version/contract/environment/availability/network/storage/trust/setup;
- `Memory read` notes with source, items, bytes, cache hit/miss, latency,
  truncation, and outcome;
- `Memory write` notes with trigger, item/byte counts, queued/committed/failed/
  cancelled/unreported state, and latency;
- sync queue depth, last prefetch/sync outcome, safe error codes, CPU time, and
  RSS where the OS reports them; and
- literal actions routed only to the manifest-declared `memory` command.

A write is `committed` only when provider JSON explicitly reports `committed`,
`durable`, or an equivalent committed state. Generic success never implies
durability. Cancellation and timeout are never replayed; provenance says the
outcome may be ambiguous.

Presentation retains content-free operational metadata only. Owner-specific
updates are correlated to the active parent request or carry the exact complete
host-derived owner triple when emitted by a background worker; they are never
downgraded to process-global state. The only process-global snapshot is the
initial Off/discovery metadata view. Complete snapshots are coalesced fairly
across owners to at most 30 updates/second (below the host's 32/second ceiling).
Reconnect/resync returns the current complete snapshot without prefetching,
writing, reinitializing, or repeating an action. The host process-generation
fence removes stale state.

## `/memory` headless fallback

The same resident state is inspectable in plain/headless modes:

```text
/memory
/memory status
/memory list
/memory snapshot
/memory show <provider-id>
/memory trust <provider-id> <fingerprint>
/memory select <provider-id>
/memory off
/memory retry
/memory reload
/memory discover
/memory lifecycle
```

No-argument `/memory` is the textual provider picker fallback. `snapshot` emits
the exact generic presentation snapshot. Read-only inspection never imports,
initializes, retries, reads, writes, or refreshes provider state.

## Default bounds

| Resource | Default | Package maximum |
|---|---:|---:|
| discovered providers | 32 | 64 |
| selected-provider tools | 32 | 64 |
| query text | 16 KiB | 32 KiB |
| aggregate context | 32 KiB | 48 KiB |
| tool result | 32 KiB | 64 KiB |
| owners | 8 | 32 |
| background queue per owner | 16 | 64 |
| availability check | 1 s | 5 s |
| initialize | 5 s | 30 s |
| prefetch | 3 s | 30 s |
| tool call | 30 s | 120 s |
| sync/hook call | 5 s | 30 s |
| aggregate queue drain + all provider shutdowns | 1 s | 5 s |
| retained activities | 64 | fixed |
| retained session snippets | 32 × 16 KiB | fixed |

Provider calls run on capped daemon workers and are waited with cancellation and
deadlines. Python cannot safely kill an arbitrary provider thread, so an
uncooperative timeout, cancellation, or shutdown immediately terminates the
entire extension process generation instead of detaching work or reporting a
false terminal state. Ygg's supervised process replacement/process-group cleanup
is the fence; the interrupted call remains ambiguous and is never replayed.

## Health and recovery

Compact state is `off`, loading, active provider, syncing, degraded, unavailable,
or stopped. A provider failure never makes direct coding unavailable.

- `/memory retry` re-runs availability/initialization at an idle boundary.
- `/memory reload` shuts down and reconstructs the selected provider, refreshes
  the frozen system block, and republishes schemas.
- `/memory discover` rereads metadata only. A changed fingerprint degrades the
  active selection until it is reviewed and explicitly trusted/reloaded.
- `/memory off` fences queued work, unregisters the focused catalog, and shuts
  down the owner instance. It does not delete provider data.
- `/extensions reload` remains the process-generation replacement boundary.

Uninstalling the bundle removes package files only. Provider environments,
configuration, credentials, stores, indexes, and models are outside this package
and remain untouched.

## Fixtures and tests

`fixtures/providers/mock_provider` is an adversarial local conformance fixture.
`fixtures/providers/offline_provider` is a realistic **test-only** network-free
lexical provider that creates a provider-owned JSON store inside a temporary
Hermes home during tests. `fixtures/hermes_environment` contains only a minimal
ABC/distribution/entry-point test environment; none is a supported production
provider or runtime dependency.

`fixtures/presentation` covers Off/discovery/selection/switching, read, sync
queue/acceptance, queued/committed/failed write, degraded/redacted health,
reconnect without replay, owner isolation, and stale-generation cleanup.

Run the dependency-free suite from this source root:

```console
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -t . -v
```

The suite covers strict config and credential-file handling, exact contract/
interpreter mismatch, metadata-only directory and entry-point discovery,
fingerprint changes, selected-only import, one-provider-per-owner isolation,
dynamic tools, malformed schemas/results, injected and oversized memory,
timeout/cancellation, lifecycle mappings, queue bounds, graceful shutdown,
generic presentation, full API `0.2` wire behavior, byte-for-byte SDK sync, and
reproducible release archive smoke.
