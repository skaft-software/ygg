# First-class MCP qualification — #179

**Not qualified for production remote MCP or full Codex parity.** This is the
source-pinned acceptance matrix for [#179](https://github.com/skaft-software/octet/issues/179),
not a Shopify adapter specification, a completed test report, or permission to
remove the experimental HTTP gate.

[qualification/baseline.json](qualification/baseline.json) is the machine-readable
ledger: 32 stable feature IDs, 17 journeys, nine retained transport defects and
nine release gates. It separates upstream behavior, public protocol requirements,
Octet implementation status, and evidence of actually executed journeys.
[README](README.md) and [REFERENCE](REFERENCE.md) remain the package usage and
implementation contracts. Existing local stdio must be **verified, not rebuilt**;
local-stdio computer-use integrations do not depend on qualifying remote HTTP.

## Immutable baselines

| Baseline | Pin | Meaning |
| --- | --- | --- |
| Codex | [`6baa076eb692b73ede097b4870dc45692c7556d9`](https://github.com/openai/codex/tree/6baa076eb692b73ede097b4870dc45692c7556d9), 2026-09-10 | Actual client/product routines and tests, not names of files or SDK capabilities alone. |
| Public MCP | [`aa8ce049f089f92618340190d4ece141f663310d`](https://github.com/modelcontextprotocol/modelcontextprotocol/tree/aa8ce049f089f92618340190d4ece141f663310d), 2026-09-08 | Latest public version in this resolved snapshot: `schema/2026-07-28` and `docs/specification/2026-07-28`. |
| Codex dependency | `rmcp 3.2.0` | Registry archive SHA-256 `42b6914fac0be956fe704a38239c3f44a9f841d1b06a5713d2f638065593f5b5`, from the pinned Cargo lockfile; source inspected in memory, not installed. |
| Octet input | `207dccc6dcd8999db9cc6b41f87e4e86448d02aa` | Initial source assessment before this implementation stack. Candidate fixes/results must be recorded separately, not attributed to this snapshot. |

Each JSON `sources` entry has an immutable repository/path URL, Git blob SHA-1,
content SHA-256, reviewed line ranges and the substantive finding. The two rmcp
archive entries have the archive and member SHA-256 instead of a repository blob.
Source IDs such as `C-MODE`, `M-HTTP` and `O-REFERENCE` resolve there. The mutable
issue body has its own observed digest; the original Wix reproduction is
historical evidence, not proof of current service availability.

**Codex itself does not default to the newest protocol.**
[`protocol_mode.rs:9-49`](https://github.com/openai/codex/blob/6baa076eb692b73ede097b4870dc45692c7556d9/codex-rs/rmcp-client/src/protocol_mode.rs#L9-L49)
selects legacy initialization by default, with 2026 discovery available in an
explicit mode. Stdio additionally requires an explicit per-server marker.
[`features/src/lib.rs:1309-1326`](https://github.com/openai/codex/blob/6baa076eb692b73ede097b4870dc45692c7556d9/codex-rs/features/src/lib.rs#L1309-L1326)
marks modern MCP and coordinated OAuth refresh under development, disabled by
default. Include those supported conditional paths in the target matrix, but do
not describe them as universally enabled or as certified public conformance.

### What is genuinely in scope?

| Feature IDs | Source-backed boundary | Initial Octet assessment |
| --- | --- | --- |
| `MCP-001`, `009`, `026` | Resident stdio, catalogs/epochs and conservative host action policy | Implemented for the described legacy scope; candidate regressions unrun here. |
| `MCP-002`–`008`, `010`, `020` | Dual-era lifecycle, remote framing/session/recovery, metadata/headers, subscriptions, freshness and notifications | Legacy pieces partial; modern behavior needs substantive implementation. |
| `MCP-011`–`012` | General schemas and ordered text/structured/media/resource content | Partial; bounded text/image/audio is not all MCP content. |
| `MCP-013` | Resources, templates and reads | Missing; **not excludable as tools-only scope**. |
| `MCP-015`–`017` | Standard form/URL elicitation and modern multi-round-trip requests | Missing; **not excludable as protocol optionals unsupported by Codex**. |
| `MCP-021`–`025` | Unauthenticated/bearer access and OAuth discovery, consent, registration, issuer/resource binding and credential lifecycle | Experimental bearer seam only; stock OAuth is missing. |
| `MCP-027`–`028` | Nine-defect remediation and actual product/frontends | Safety partial; product journeys need live evidence. |
| `MCP-014`, `018`–`019`, `029`–`030` | Prompts/completions, sampling, roots, standalone legacy HTTP+SSE and tasks | Protocol optional, not established first-class surfaces of the reviewed Codex client; Octet unsupported. |
| `MCP-031`–`032` | Enterprise authorization and explicitly negotiated Codex-specific forms/verification/app extensions | Baseline-supported conditional inventory, missing in Octet; not mandatory core MCP features. |

Resources are real product support: the pinned Codex
[`connection_manager/resources.rs`](https://github.com/openai/codex/blob/6baa076eb692b73ede097b4870dc45692c7556d9/codex-rs/codex-mcp/src/connection_manager/resources.rs)
invokes resource/template listing and reads. Its
[`rmcp_client.rs:729-785`](https://github.com/openai/codex/blob/6baa076eb692b73ede097b4870dc45692c7556d9/codex-rs/rmcp-client/src/rmcp_client.rs#L729-L785)
selects modern resource-read MRTR. The inspected
[`mcp_2026_mrtr.rs`](https://github.com/openai/codex/blob/6baa076eb692b73ede097b4870dc45692c7556d9/codex-rs/rmcp-client/tests/mcp_2026_mrtr.rs)
contains actual form/URL and resource continuation assertions; these upstream
tests were **not run** as Octet qualification.

Conversely, Codex initializes default client capabilities plus elicitation and
selected extensions (`C-CAPS`). Its ordinary handler does not implement sampling
or application roots (`C-HANDLER`): rmcp's default sampling returns method-not-found
and roots returns an empty list (`R-HANDLER`), neither capability advertised.
Prompt-list notification logging, SDK `get_prompt` helpers and a generic custom
request method do not establish a first-class prompts product journey. Preserve
these negative findings explicitly instead of inventing support or quietly
dropping rows. Revisit them only with concrete exposed call-site evidence.

The SDK also exposes `subscriptions/listen`, while the reviewed Codex wrapper
does not expose a first-class listener. That is a **public 2026 protocol pattern**
to qualify, not evidence Codex has already qualified a notification UI. Codex
explicitly disables rmcp response caching/stale-on-error (`C-CLIENT:1333-1340`);
optional caching must not be invented as a parity requirement.

Enterprise ID-JAG exchanges and capability-gated OpenAI-native verification are
actual conditional upstream implementations, not fabricated features. Keep their
absence visible for any entire-Codex parity claim. They neither waive standard
OAuth/elicitation nor require importing proprietary connector UI or organizational
account setup into the core client deliverable. An intentionally narrower release
claim must name its exclusions; it cannot silently redefine #179 as Shopify-only.

## Why changing version constants cannot implement 2026 compatibility

The pinned [public changelog](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/aa8ce049f089f92618340190d4ece141f663310d/docs/specification/2026-07-28/changelog.mdx)
and actual schema/transport sources require different behavior:

1. **Two lifecycle eras.** Modern MCP removes `initialize` and
   `notifications/initialized`. Construct self-contained request `_meta` with
   `io.modelcontextprotocol/protocolVersion` and `clientCapabilities`; identify
   client/server appropriately without trusting server identity as authority.
   Handle `server/discover`, advertised versions/capabilities and modern error
   codes `-32020`, `-32021`, `-32022`. Preserve legacy paths, including supported
   2025-11-25 fallback, rather than relabeling a 2025-06-18 handshake.
2. **Different HTTP metadata.** Send matching `MCP-Protocol-Version`,
   `Mcp-Method`, applicable `Mcp-Name`, and schema-derived `Mcp-Param-*` headers.
   Validate `x-mcp-header` placement, primitive types and case-insensitive
   uniqueness; exclude invalid tools. Encode unsafe/sentinel values, omit
   missing/null parameters and retain bounded catalog-schema provenance. The
   existing schema vocabulary filter otherwise drops needed annotations.
3. **No modern protocol sessions or SSE resume.** Do not send legacy
   `Mcp-Session-Id`, `Last-Event-ID`, or session GET/DELETE in modern mode. A modern
   method-not-found 404 is not an expired legacy session. Modern HTTP cancellation
   closes the response stream; stdio still sends `notifications/cancelled`.
   Retain and qualify the legacy mechanisms only on their legacy wire.
4. **Result discriminators and MRTR.** Parse `resultType: complete` versus
   `input_required`, reject unknown types, and treat absent type as complete for
   earlier-protocol servers. Drive only eligible operations (`tools/call`,
   `resources/read`, and `prompts/get` if implemented) with bounded rounds/time,
   capability-checked input handling, fresh request IDs and exact opaque
   `requestState` scoped to that operation. **The current schema-less result
   lowerer can otherwise turn an input-required reply into empty successful
   content.** Returning method-not-found to legacy reverse requests does not
   implement modern continuation.
5. **Subscriptions, freshness and notifications.** Implement bounded
   `subscriptions/listen` response streams, acknowledgment/filter checking,
   `subscriptionId` correlation and re-establishment. Legacy list-change callbacks
   alone are insufficient. Handle `ttlMs`/`cacheScope`; caching is optional, but
   private state must never cross authorization contexts and MRTR retries must
   not be cached. Modern logging is request-scoped via `_meta` log level;
   `logging/setLevel`, `ping` and roots-list-change signaling are removed.
6. **Broader schemas/content.** Preserve general JSON Schema semantics, including
   bounded reference/composition handling, or explicitly reject unsupported
   schemas; do not silently weaken them. Octet already serializes arbitrary JSON
   structured values, so that is not a new object-only restriction. Its basic
   schema validator and rejection of resource links/embedded resources remain
   substantive gaps. Resources and standard form/URL elicitation need host-facing
   implementations as well as wire parsing.
7. **First-class authorization.** Implement bounded RFC9728 protected-resource
   and ordered OAuth/OIDC discovery, S256 PKCE/metadata checks, manual consent and
   callback validation, exact issuer/resource binding, pre-registration/CIMD/DCR,
   secure owner-bound credentials, refresh/expiry/revocation/logout and scope
   challenge handling. DCR is deprecated, not sufficient as the only supported
   registration path. Octet needs its **own** reviewed CIMD metadata identity;
   never reuse Codex's client ID.

Public requirements and Codex compatibility heuristics are not identical. For
example, Codex's discovery fixtures deliberately require particular correlated
legacy/version evidence, while the public transport describes broader
non-modern-error fallback. Codex's ordinary legacy OAuth mode permits absent
issuer metadata; current public metadata validation is stricter. Record and
resolve these differences instead of treating either fixture names or a single
successful request as conformance.

The public modern transport describes a lost request being re-issued with a new
ID. That is **not authority to automatically replay ambiguous actions** in Octet.
Surface loss/ambiguity, retain host policy, and distinguish a newly authorized
retry from a successful `input_required` continuation. Cancellation never promises
rollback. The existing fail-closed policy and #383 approval boundary remain intact.

MCP protocol version is also independent of Octet extension API version. The
bundle uses supported API `0.2`; [current extension documentation](../../docs/extensions.md)
explicitly defers API `0.3` dynamic tools and media/legacy services. Retagging the
manifest cannot supply those host contracts or repair MCP compatibility.

## A fixed regression is not a qualified journey

| Evidence | What it proves | What it does not prove |
| --- | --- | --- |
| Source inspection | A pinned routine/capability or demonstrated gap exists | That tests pass or a shipped client connects. |
| A targeted defect fix plus a passing regression | The specified controlled failure is prevented on the tested candidate | That all nine defects, real DNS/TLS, auth or remote lifecycle qualify. |
| Existing subprocess/loopback fixtures | The observed deterministic framing/policy/catalog path works | Account consent, external sessions or production remote safety. |
| Actual version-matched product journey | The named frontend/endpoint/auth/owner path worked at the recorded time | Other routes, scopes, accounts, transports or unrun journeys. |

The `D-HTTP-01`–`D-HTTP-09` entries retain the original defect order: DNS rebinding,
owner-state sharing, uncancellable DNS workers, buffered SSE peer identity,
control-message fanout, aggregate budget resets, truncated frames, empty event-ID
cursor handling, and startup deadline escape. Each needs an implementation fix
and its own targeted evidence. Keep the [experimental warning](REFERENCE.md#known-streamable-http-defects)
until applicable gates close; deleting the warning is not remediation.

Run the existing package regressions from this package root when reviewing a
candidate (the baseline author has **not** run them):

```console
python3 -B -m unittest discover -s tests -t . -v
```

Validate the ledger offline with `python3 -B qualification/check.py`. In a Git
checkout, add `--verify-local-snapshot` to verify the pinned Octet source hashes
and reviewed ranges. `--require-qualified` deliberately fails while release gates
remain open. Integrity validation is not runtime or live-provider qualification.

Add focused 2026/auth/safety fixtures for demonstrated gaps, and run appropriate
host/frontend/release checks for changed paths. Record failures as failures;
skipped, unavailable and unrun journeys never count as passed. A generic tool
bridge must preserve host-derived owner/generation fences, provenance and
headless denial. Do not weaken policy to make a fixture or public store tool run.

## Required real-world journeys remain distinct

| Journey | Target / authority | Qualification boundary |
| --- | --- | --- |
| `J-SHOPIFY-BASE` | Explicit reviewed storefront `/api/mcp`, **auth omitted** | Generic unauthenticated MCP; no Shopify-specific adapter, customer login or purchase. |
| `J-SHOPIFY-UCP` | Same explicitly reviewed storefront class, `/api/ucp/mcp`, **auth omitted** | Separate MCP route/result; ordinary `/api/mcp` or non-MCP UCP metadata is not evidence for it. |
| `J-WIX` | Historical original #179 endpoint `https://mcp.wix.com/mcp`, **authenticated user journey** | Retain account authorization, catalog/read, supported session/recovery/cancellation and logout acceptance; public Shopify cannot substitute. |
| `J-AUTH-LIFECYCLE` | Separately authorized generic remote MCP resource | Registration/consent/token lifecycle, secure restart, refresh and revocation evidence. |
| `J-CUSTOMER-OAUTH` | Separately reviewed customer-account resource/issuer/scopes | Customer authorization is not public storefront access, transport bearer reachability, Wix authorization or checkout permission. |

No exact storefront was supplied and no live MCP endpoint calls or account
authorization were allowed for this source assessment. Shopify route names are
requested qualification targets, **not observed vendor behavior**. Vendor
documentation lookups at guessed documentation URLs returned 404 and were not
used as evidence. Wix may have changed since the historical report; an unavailable
endpoint/account must remain an explicit blocker, not an invented pass or a quiet
substitution. Customer OAuth requires its own owner-authorized setup before any
customer feature can be claimed.

Future manual runners must use user-held credentials outside model context and
committed evidence. Even a harmless-looking vendor tool must pass Octet's exact
read-only classification or negotiated approval policy. Unknown/destructive calls
stay denied without approval issuance; auth and endpoint trust do not authorize
cart, checkout, order, payment or account mutation.

## Recording candidate evidence and release decisions

The JSON is bounded to 128 KiB, 64 sources, 48 feature rows, 24 journeys, 12 gates
and 4 KiB per string. Keep stable IDs; attach small sanitized evidence references,
not raw transcripts, request bodies, secrets, account identifiers or tool output.
For each candidate evidence record retain:

- immutable candidate commit and package/build hashes (or working-tree diff hash);
- command/test selector, OS/frontend, date and actual terminal outcome;
- relevant `MCP-*`, `D-HTTP-*`, `J-*` and `G-*` IDs;
- opaque sanitized session/artifact reference and evidence SHA-256;
- for manual remote runs, exact approved endpoint/issuer and observed protocol,
  transport, registration/auth mode, scopes and recovery/cancellation outcome;
- concrete remaining blocker, without exposing credentials or personal data.

`current_octet.status` initially assesses the pinned input, not another worker's
uncommitted fixes. Append candidate evidence and update assessments after actual
inspection/tests, preserving original pins and source findings. `implemented`
means implementation exists for the stated scope; `qualification` and journey
`result` independently record execution. A feature can have fixed tests and still
need live evidence. Upstream source tests are never candidate test results.

The release gates separately cover source integrity (`G-SOURCE`), stdio
regression, HTTP safety, 2026 semantics, authorization, supported surfaces, real
remote journeys, product integration and truthful claims. **All applicable gates
must close before production remote or full-scope parity is claimed.** Conditional
non-core extensions must remain visible as exclusions if not claimed; standard
resources/elicitation/auth are not optional exclusions from this baseline scope.
Customer/enterprise claims require their own journeys. Keep #179 open if its
supported auth/session/Wix acceptance remains unrun, even after local fixes pass.

This source matrix records no completed MCP journeys and authorizes no network,
account, install, credential, commit, push or issue-modification actions.
