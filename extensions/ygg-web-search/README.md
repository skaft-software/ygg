# ygg-web-search

`ygg-web-search` is an opt-in API `0.2` executable extension for bounded web
search and public-page retrieval with stable citations. It supports
[Brave Search API](https://brave.com/search/api/) (recommended) and an explicitly
configured [SearXNG](https://docs.searxng.org/) JSON endpoint. It is search and
fetch only: it has no browser tabs, cookies, login, JavaScript, form submission,
image search, crawler, or computer-use authority.

The bundle is inert after installation. Discovery does not execute it, install
does not enable or trust it, and `--safe-mode` keeps it stopped. A running
extension has the current user's operating-system authority; the manifest's
network and fixed user-state declarations are visible consent metadata, not a
sandbox.

## Install and opt in

Official release bundles install with:

```console
ygg extension install ygg-web-search
```

A local checkout can be selected without installing:

```console
ygg --extension-dir ./extensions \
    --enable-extension ygg-web-search \
    --trust-extension ygg-web-search
```

For an installed global bundle, explicitly enable and trust it for an
invocation:

```console
ygg --enable-extension ygg-web-search --trust-extension ygg-web-search
```

Or add both names to user configuration:

```toml
enabled_extensions = ["ygg-web-search"]
trusted_extensions = ["ygg-web-search"]
```

The bundle includes `skills/ygg-web-search/SKILL.md`. Bundle skill discovery is
safe and inert; load it explicitly with `/skills load ygg-web-search` when web
research is wanted. The runtime contains its dependency-free Python SDK, so
installation performs no `pip install`, model call, service setup, or other
arbitrary code. Python 3.9 or newer must be available as `python3`.

Release compatibility is recorded directly in `extension.toml`: bundle
`0.3.0`, extension API `0.2`, exact Ygg `0.6.5`.

## Choose a provider

Open `/extensions` and select `ygg-web-search`. Enabling a running trusted copy
opens a second picker using the same interface:

1. **Brave Search (recommended)**
2. **SearXNG**

Selecting an already enabled `ygg-web-search` opens that provider picker again
and also offers to disable the extension. `/web-search status`,
`/web-search setup brave`, `/web-search setup searxng`, and
`/web-search logout` provide keyboard/scriptable fallbacks.

### Brave Search (recommended)

On first selection, Ygg shows the key-management link and opens a private input
surface for the API key:

<https://api.search.brave.com/app/keys>

The key is stored at `~/.ygg/credentials/ygg-web-search-brave.key` as an
owner-private regular file. It is never placed in ordinary configuration,
queries, URLs, tool results, status, presentation state, or diagnostics. Brave
requests use the fixed HTTPS endpoint and `X-Subscription-Token`; credentialed
requests never follow redirects. If setup was skipped, the first `web_search`
call asks for the key through the same private input channel. An HTTP 401/403
invalidates the stored key so the next setup/search can ask again.

### SearXNG

Existing `~/.config/ygg/ygg-web-search.json` SearXNG configurations continue to
work unchanged. The provider picker preserves those settings while Brave is
selected and restores them when switching back. If no SearXNG endpoint has been
configured, setup asks for one.

Manual configuration remains available:

```console
mkdir -p ~/.config/ygg
cp ~/.ygg/extensions/ygg-web-search/config.example.json \
   ~/.config/ygg/ygg-web-search.json
$EDITOR ~/.config/ygg/ygg-web-search.json
```

For a source checkout, copy `config.example.json` from this directory instead.
The complete schema is `config.schema.json`. The runtime accepts a regular,
non-symlink UTF-8 JSON file of at most 64 KiB only when it is owned by the
current user and is not group- or world-writable; unknown fields are rejected.
Configuration changes are adopted only when no extension tool operation is
active; changing a file never changes an in-flight request.

Minimal SearXNG configuration:

```json
{
  "version": 1,
  "provider": {
    "kind": "searxng",
    "endpoint": "https://search.example.org/search",
    "label": "SearXNG",
    "allow_private_endpoint": false
  }
}
```

The SearXNG instance must permit JSON search responses (`format=json`). Its
endpoint is deliberately non-secret; credential-bearing URLs and unknown
credential fields are rejected. A self-hosted endpoint on loopback or a private
address requires `allow_private_endpoint: true`. That exception applies only to
the configured provider hostname. Model-supplied `web_fetch` and `web_find`
destinations and every redirect remain public-address-only.

Optional `limits.allowed_domains` is an egress allowlist for search results,
`web_fetch`, `web_find`, and redirects. A tool-supplied `domains` list can narrow
that configuration but cannot widen it.

## Tools and bounds

| Tool | Purpose | Hard bounds |
| --- | --- | --- |
| `web_search` | Query the selected Brave Search or SearXNG adapter | 512-byte query, 5 requested domains, 10 results, 20 seconds, 512 KiB provider response; Brave credentialed requests do not redirect |
| `web_fetch` | Normalize one public HTML or plain-text URL | HTTP(S) ports 80/443, 20 seconds, 3 redirects, 512 KiB download, 128 KiB normalized content |
| `web_find` | Find a literal pattern and return excerpts | 256-byte pattern, 20 matches, 512-byte excerpts, the same fetch bounds as `web_fetch` |

Defaults can be made smaller in configuration. Call arguments may select a
smaller result, byte, redirect, or time limit but cannot exceed the hard limit.
HTML normalization drops script, style, template, SVG, canvas, and noscript
content. Direct retrieval accepts only HTML, XHTML, and plain text and requests
identity transfer encoding; compressed, oversized, unsupported, or malformed
responses fail explicitly.

Every URL is normalized to HTTP(S), stripped of fragments and common tracking
parameters, sorted deterministically, limited to 2048 bytes, and rejected if it
contains credentials. Before each connection—including each redirect—the
runtime resolves and rejects unspecified, loopback, private, link-local,
multicast, and reserved destinations. HTTPS-to-HTTP redirects are rejected.
Connections are pinned to an address from that validated resolution rather than
asking the HTTP layer to resolve the name again.

A stable citation such as `[web-0123456789abcdef]` is the first 16 hexadecimal
characters of SHA-256 over the sanitized URL. Ranking, title, snippet, and cache
state do not affect it. Search results with duplicate citation IDs, invalid URLs,
private literal addresses, or out-of-allowlist domains are omitted and the
terminal result is marked partial.

## Trust, egress, and result visibility

Search sends the query and selected domain filters to the selected provider:
the fixed Brave Search API endpoint or the configured SearXNG service. Open/find
sends the sanitized URL to that public origin and performs normal DNS and TLS
traffic. Both search services remain external and are neither vendored nor
managed by Ygg.

Every successful text result begins with an explicit **UNTRUSTED WEB DATA**
frame. Titles, URLs, snippets, excerpts, page text, and publication metadata are
data only. They cannot change Ygg policy, enable tools, grant trust, authorize a
command, or justify a side effect. The packaged skill repeats that rule.

Each call returns one API `0.2` terminal result with:

- compact model-visible text and validated `structured_content`;
- normalized citation ID, title, sanitized URL/origin, snippet or content, and
  publication metadata when the source supplied it; and
- non-model-visible metadata containing adapter/source provenance, cache
  hit/miss, operation, provider label, result count, normalized bytes, latency,
  redirects, truncation, and terminal outcome.

Provider response bodies, endpoint configuration, and credentials are never
copied into health, progress, or diagnostics. Query and retrieved content remain
only in ordinary tool arguments/results, not compact status or activity labels.

## Cache, cancellation, health, and offline behavior

The extension keeps only a bounded process-local TTL/LRU cache (at most 64
entries, 2 MiB, and 15 minutes; defaults are 64 entries/2 MiB/5 minutes). It is
cleared on configuration changes and process replacement and never written to
disk. Exact repeated search/open requests report a non-model-visible cache hit
and avoid another network request. An unexpired hit can still succeed while the
network is offline; otherwise offline, timeout, rate-limit, provider failure,
unsupported content, oversized content, and blocked destination are explicit
terminal states. None prevents ordinary coding tools from continuing.

API `0.2` cancellation is cooperative and request-scoped. The runtime checks the
ambient token before/after resolution, redirects, normalization, and every
bounded read, and caps individual socket waits below the host's cancellation
grace. Cancellation settles with JSON-RPC `-32800`; it does not claim rollback
or emit a second tool result. Progress is request-scoped, monotonic, bounded,
and contains only provider/stage/count/byte information—not queries, URLs, or
retrieved text.

The optional status contribution is compact and passive:

- `web · Off` when no provider configuration exists;
- `web · Brave Search setup required` when Brave is selected without a key;
- `web · <label>` when configured/ready; and
- an explicit authentication, degraded, offline, or rate-limited state after failure.

It performs no health-check request and never exposes the endpoint.

## Frontend-neutral presentation

`fixtures/presentation/` contains bounded semantic fixtures for disabled,
configured, progress, citation detail, cache/truncation, cancellation, provider
failure, offline, reconnect-without-refetch, and stale-generation cleanup. They
contain semantic status/activity/list/detail data only—no ANSI, HTML,
JavaScript, CSS, queries, snippets, or page content.

The runtime publishes complete monotonic API `0.2` `presentation/update`
snapshots in addition to status, progress, structured results, and retained
metadata. Its activity summary carries operation/provider/progress/count/bytes,
cache, latency, truncation, and outcome without copying a query or retrieved
body. Its citation collection/detail uses stable node IDs and host-vetted `url`
references. Query strings and fragments remain only in the immutable tool result
and are stripped before a source link enters retained frontend/reconnect state.
Generic host reducers fence updates by process generation, retain a
terminal snapshot across Serve reconnect, remove replacement-generation state,
and render the same data without a search-specific frontend state machine.
Source URLs are exposed only as host-validated references requiring an explicit
user click; `web_fetch` is retrieval, not a browser tab.

## Test

All tests use only local HTTP fixtures and the bundled SDK:

```console
python3 -m unittest discover -s extensions/ygg-web-search/tests -v
```

They cover normalization, untrusted framing, redirects and private-address
rejection, truncation and oversized responses, unsupported content,
cancellation, timeout, provider failure, stable citations, caching, health,
protocol negotiation/progress, semantic fixtures, and a self-contained release
smoke test.
