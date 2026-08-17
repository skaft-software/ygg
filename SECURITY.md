# Security policy

## Supported versions

Ygg is pre-1.0 software. Security fixes are made on the latest `0.4.0` release; older snapshots are not supported.

## Boundary and defaults

Ygg runs as the current operating-system user. It is **not** an OS sandbox:
any effect admitted through the explicit unsafe-host policy inherits the
process's filesystem, environment, subprocess, and network authority. Use an
isolated account, container, VM, or platform sandbox when a repository or model
endpoint is not trusted.

Ygg nevertheless treats its own policy and persistence boundaries as security invariants:

- Explicit built-in file paths are workspace-only by default. Unix file reads and mutations use descriptor-relative, no-follow operations so validation cannot be redirected by a parent-symlink replacement.
- Project `.ygg/config.toml`, workspace `AGENTS.md`, and workspace skills are ignored unless the user passes `--workspace-trusted`.
- Trusted project settings may tighten global authority/resource floors but cannot relax them. Environment and explicit CLI settings remain user-controlled higher-trust layers.
- Every model-requested tool call passes through a host-owned effect broker. The default Controlled policy forces built-in file operations to workspace-relative paths, permits pure and workspace-read effects, requires a one-shot interactive approval for workspace mutation, and denies host reads/mutations, network, delegation, extension, and unknown effects. `bash` is a controlled process-capable tool but still requires explicit per-call approval in this mode; all other native process effects remain denied. It also prevents executable-extension process startup itself, regardless of the lower-level process/shell gates. `unsafe_host_effects = true`, `YGG_UNSAFE_HOST_EFFECTS=true`, or `--unsafe-host-effects` opts into ambient host authority; trusted project config cannot grant that opt-in. Workspace-mutation grants bind the exact principal, run, catalog generation, provider call ID, tool, classification, arguments, and policy version; they are short-lived, atomically single-use, reserved before hooks, and consumed immediately before execution.
- Context/config/credential files must be bounded regular files. Workspace context symlinks and special files are rejected.
- Disabled tools are removed from both the provider schema and execution registry. `--no-edit` disables `edit` and `write`; `--tools read,search` and `--no-tools` provide complete allowlisting.
- Arbitrary process execution and shell execution are treated as equivalent authority. `bash` requires both compatibility gates, and in `Controlled` mode it also requires explicit approval before each execution.
- Crash replay requires both a tool's static replay-safe declaration and an exact `Pure` or `WorkspaceRead` host classification. Every other unresolved call is paired with an indeterminate result and is not executed.
- Session mutation uses advisory interprocess locking, stale-generation checks, private permissions, bounded parsing, and synced records. Session listing is byte-for-byte read-only.
- The native `ygg-host` keeps stdout protocol-only and bounds inbound and outbound NDJSON frames. It confines resumed sessions to regular files in the selected session directory, validates inline-provider and image inputs, denies headless tool confirmations, and cancels typed input requests. Protocol v1 has no in-band abort, so consumers must launch each host in a dedicated process group and terminate that group to cancel it.
- On first use, Ygg may copy bounded Codex credentials from owner-only legacy Codex or Hamr stores into its own owner-only store under a cross-process lock. It never modifies or deletes the legacy source and does not include credential values in migration diagnostics.
- Serve project browsing and editing resolves opaque project IDs to a revalidated root identity, then uses descriptor-relative no-follow traversal. Atomic writes recheck target identity and sync both content and the owning directory.
- Serve owns Git and PTY process groups through bounded graceful/forced cleanup, including descendants that retain output descriptors. This prevents ordinary timeout/shutdown leaks; it does not restrict what an enabled command may access.
- Serve permanent deletion journals intent before removing the transcript, retries idempotent sidecar cleanup after interruption, retains payloads referenced by another session, and fails before commit when a required store is unavailable. Conversation-content-free append-only inference accounting remains host-level history.
- Provider streams, discovery responses, context, configuration, credentials, sessions, tool arguments/results, and local file reads have hard aggregate limits.
- Serve accepts only bounded, classic single-revision PDFs for partial text extraction. An iterative raw-syntax preflight rejects excessive direct nesting before the audited `lopdf 0.42.0` parser runs; parser version, object/page/decompression limits, and a deeply nested regression are release gates.
- Run cancellation reaches provider streaming, retry waits, tools, and autonomous compaction. Once cancellation wins a request race, no summary or usage record from that request is committed.

These controls reduce accidental authority and defend documented Ygg boundaries.
Controlled is an admission policy, not full malicious-worker containment: permitted
workspace reads can enter provider-visible context, approved mutations affect the
live workspace, and Ygg does not yet provide overlay promotion, information-flow
labels, a native-code isolation backend, or a dedicated egress/secret broker.
Controlled denies most effect classes that require those missing boundaries, while
`bash` remains a process-capable tool but still requires explicit per-call
approval. They do not contain an effect admitted by the unsafe-host opt-in. In
particular, an admitted `bash` call can read credentials, access the network, and
start descendants with the user's authority.

## Recommended untrusted-repository workflow

Use OS isolation and expose only the repository copy that may be changed. At minimum:

1. Start a disposable container/VM or restricted user account with no personal credentials.
2. Mount only a disposable workspace; do not mount SSH, cloud, browser, package-registry, or provider credential directories.
3. Restrict outbound network to the selected model endpoint, or use a local endpoint.
4. Run without project resources and without commands initially:

   ```sh
   ygg --offline --no-context-files --tools read --workspace /workspace
   ```

5. Inspect project instructions/config before choosing `--workspace-trusted` or enabling mutation/command tools.

Controlled forces `allow_external_paths=false` for built-in file operations.
That path gate does not constrain paths opened by a child process admitted under
UnsafeHost; only the OS isolation boundary can do that.

## In scope

Please report, among other issues:

- bypass of workspace path, project trust, tool allowlist, cancellation, stream/resource, credential, or session-integrity guarantees;
- unauthorized disclosure caused by Ygg loading or transmitting a local resource;
- session corruption or silent duplicate mutating work;
- secret exposure in logs/errors;
- terminal control-sequence injection in terminal-safe modes;
- remotely reachable dependency vulnerabilities with demonstrated impact;
- privilege-boundary crossings or unauthorized remote interfaces.

Prompt injection and model mistakes remain expected risks, but a model using them to bypass a configured Ygg boundary is in scope.

## Private reporting

Do not open a public issue for a suspected vulnerability. Use GitHub private vulnerability reporting:

**https://github.com/skaft-software/ygg/security/advisories/new**

Include impact, reproduction steps or a proof of concept, affected version/commit, platform, and known mitigations. If that private form is unavailable, contact the repository owners privately through the GitHub organization before disclosing details.
