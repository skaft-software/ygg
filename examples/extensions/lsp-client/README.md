# lsp-client

Read-only LSP code intelligence as an executable extension (issue #23, Stage-1
spike scope). One model-callable `code_intelligence` tool with an operation
enum: `definition`, `references`, `hover`, and `diagnostics` (pull). The
`before_prompt` hook injects new language-server diagnostics once per change.

## Scope and boundaries

- Text-first: `read`, search, and build/test commands remain the fallback.
  Every unavailable state is a typed, bounded result, never a product failure.
- No hidden mutation: the extension only sends `didOpen`/`didChange` to keep
  the server's document view current. It never applies server-proposed edits.
- Servers are never downloaded or installed; a missing binary is a typed
  unavailable result.

## Supported servers

Configured by file suffix in `extension.py` (`DEFAULT_SERVERS`):

| Suffix | Server |
| --- | --- |
| `.rs` | `rust-analyzer` |
| `.py` | `pyright-langserver --stdio` |
| `.ts/.tsx/.js/.jsx` | `typescript-language-server --stdio` |

The server binary must already be on `PATH`.

## Behavior

- Lazy start: a server spawns on the first query for a matching suffix.
- Bounded: every request has a deadline; restart attempts are capped
  (3 consecutive failures or 10 lifetime starts per server); results are
  capped (10 definitions, 100 references, 20 diagnostics per file, 2 KB hover).
- Document sync: files are re-read before each query; edits made by `edit`,
  `write`, or shell tools are pushed with `didChange` so diagnostics reflect
  the current content, not a stale snapshot.
- Diagnostics injection: `before_prompt` contributes only diagnostics not
  previously injected; a file that becomes clean is forgotten so a regression
  re-reports. Total injection volume is capped per turn.
- Crash handling: a dead server is killed by process group and restarted under
  the bounds above; in-flight requests fail with a typed unavailable result.

## Tests

```console
python3 examples/extensions/lsp-client/test_extension.py
```

The suite runs against a fake LSP server over real stdio framing and covers
navigation, document resync, diagnostics delivery/dedup, and failure paths
(dead server, silent server, unconfigured suffix, missing files).
