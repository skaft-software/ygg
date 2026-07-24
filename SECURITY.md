# Security policy

## Supported versions

Ygg is currently an alpha. Security fixes are made on the latest `0.1.1-alpha` release; older snapshots are not supported.

## Boundary and defaults

Ygg runs as the current operating-system user. It is **not** an OS sandbox: commands inherit the process's filesystem, environment, subprocess, and network authority. Use an isolated account, container, VM, or platform sandbox when a repository or model endpoint is not trusted.

Ygg nevertheless treats its own policy and persistence boundaries as security invariants:

- Explicit built-in file paths are workspace-only by default. Unix file reads and mutations use descriptor-relative, no-follow operations so validation cannot be redirected by a parent-symlink replacement.
- Project `.ygg/config.toml`, workspace `AGENTS.md`, and workspace skills are ignored unless the user passes `--workspace-trusted`.
- Trusted project settings may tighten global authority/resource floors but cannot relax them. Environment and explicit CLI settings remain user-controlled higher-trust layers.
- Context/config/credential files must be bounded regular files. Workspace context symlinks and special files are rejected.
- Disabled tools are removed from both the provider schema and execution registry. `--no-edit` disables `edit` and `write`; `--tools read,search` and `--no-tools` provide complete allowlisting.
- Arbitrary process execution and shell execution are treated as equivalent authority. `bash` requires both compatibility gates to be enabled.
- Mutating or unknown tool calls left unresolved by a crash are never replayed automatically. They are paired with an indeterminate result for explicit reconciliation.
- Session mutation uses advisory interprocess locking, stale-generation checks, private permissions, bounded parsing, and synced records. Session listing is byte-for-byte read-only.
- Provider streams, discovery responses, context, configuration, credentials, sessions, tool arguments/results, and local file reads have hard aggregate limits.
- Run cancellation reaches provider streaming, retry waits, tools, and autonomous compaction. Once cancellation wins a request race, no summary or usage record from that request is committed.

These controls reduce accidental authority and defend documented Ygg boundaries. They do not contain a command that the user has enabled. In particular, an enabled `bash` can read credentials, access the network, and start descendants with the user's authority.

## Recommended untrusted-repository workflow

Use OS isolation and expose only the repository copy that may be changed. At minimum:

1. Start a disposable container/VM or restricted user account with no personal credentials.
2. Mount only a disposable workspace; do not mount SSH, cloud, browser, package-registry, or provider credential directories.
3. Restrict outbound network to the selected model endpoint, or use a local endpoint.
4. Run without project resources and without commands initially:

   ```sh
   ygg --offline --no-context-files --tools read,search --workspace /workspace
   ```

5. Inspect project instructions/config before choosing `--workspace-trusted` or enabling mutation/command tools.

`allow_external_paths=false` does not constrain paths opened by a user-enabled child process; only the OS isolation boundary can do that.

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
