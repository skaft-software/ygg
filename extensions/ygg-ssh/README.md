# ygg-ssh

`ygg-ssh` is Ygg's opt-in API `0.2` remote-workspace adapter for aliases that
the user already configured and authenticated through the system OpenSSH client.
It is tool-oriented and does not create an interactive terminal or remote Ygg
daemon.

```text
Ygg <- API 0.2 JSON-RPC -> ygg-ssh -> system ssh -> configured alias
```

## Security and authority

Installing or discovering this package is inert. Ygg starts it only after the
normal independent extension enablement, exact trust grant, and full-access
process gate. The package also starts with no selected target and a missing
`~/.ygg/ssh.json` is a healthy empty configuration.

The boundaries are deliberately narrow:

- A model tool **cannot provide a host, alias, user, port, ProxyJump, identity
  file, agent socket, or remote cwd**. `/ssh connect <target>` accepts only a
  stable ID from the strict user/trusted configuration. Presentation actions use
  the same manifest-declared `ssh` command and literal configured IDs.
- The adapter invokes the system `ssh` binary directly, never through a local
  shell. OpenSSH resolves the configured alias using the user's normal config,
  agent, keys, known-hosts policy, and proxy configuration.
- `BatchMode=yes` and `NumberOfPasswordPrompts=0` prohibit password, OTP, and
  keyboard-interactive collection. The extension has no password/input flow,
  never copies private keys, and never runs credential discovery.
- The manifest explicitly allowlists the ambient `SSH_AUTH_SOCK`. Ygg `0.6.0-dev`
  forwards only that named value to this extension; the default sanitized
  subprocess environment still excludes it for other tools/extensions. When it
  is absent, status reports that fact and OpenSSH may still use config-selected
  key files or a literal `IdentityAgent`. No socket path is returned or logged.
- Every connection is fenced by the host-derived API `0.2`
  `{session_id, extension_instance_id, process_generation}` owner. A changed
  owner disconnects the stale master before admitting work.
- The remote cwd and authority (`read-only` or `read-write`) are fixed in trusted
  configuration. There is no UI action or model argument that upgrades
  authority.
- V1 conservatively treats every `ssh_exec` call as a **mutation**. It is denied
  for read-only targets and requires a fresh, default-deny interactive
  confirmation for each call. `ssh_write` has the same boundary. Headless or
  unavailable confirmation fails closed.
- `ssh_read` and `ssh_list` are the only remote file operations available in
  read-only mode.
  Paths are normalized relative lexical paths. This is not a remote filesystem
  sandbox: remote symlinks and the remote account's permissions still apply.
- Agent forwarding, configured port forwarding, TTY allocation, local commands,
  and remote-command overrides are disabled for adapter-owned invocations.
  Forwarding/tunneling is not a V1 feature.

An admitted executable extension and the system SSH client run with the current
user's OS authority. Manifest declarations are visible consent metadata, not an
OS sandbox. Ygg `--safe-mode` discovers but does not start executable
extensions; it is not a way to silently enable remote execution.

## Requirements and installation

- Ygg exactly `0.6.0-dev` (`requires_ygg = "=0.6.0-dev"`)
- Python 3.9 or newer
- a system OpenSSH `ssh` client
- a non-interactive, already authenticated OpenSSH alias

The release bundle vendors the tested dependency-free Python extension SDK
under `vendor/`; install never invokes `pip`, downloads SSH software, or runs a
hook.

```console
ygg extension install ygg-ssh
ygg --enable-extension ygg-ssh --trust-extension ygg-ssh
```

From a checkout:

```console
ygg --extension-dir ./extensions \
  --enable-extension ygg-ssh \
  --trust-extension ygg-ssh
```

## OpenSSH setup

Configure and test the alias outside Ygg first. Ygg owns neither this file nor
its keys/agent/known-hosts decisions:

```sshconfig
# ~/.ssh/config
Host docs-prod
    HostName docs.internal.example
    User deploy
    IdentityAgent ~/.ssh/agent.sock
```

Verify that it succeeds without a prompt:

```console
ssh -o BatchMode=yes docs-prod true
```

`ygg-ssh` does not accept `HostName`, `User`, `Port`, `ProxyJump`,
`IdentityFile`, or `IdentityAgent` in its JSON configuration. Put those in the
OpenSSH alias so they remain user-owned and cannot come from model text.

## ygg-ssh configuration

The default file is `~/.ygg/ssh.json`. Copy the disabled example, protect it,
and validate it without making a connection:

```console
mkdir -p ~/.ygg
cp extensions/ygg-ssh/config.example.json ~/.ygg/ssh.json
chmod 600 ~/.ygg/ssh.json
$EDITOR ~/.ygg/ssh.json
extensions/ygg-ssh/ygg-ssh --config ~/.ygg/ssh.json --check-config
```

Minimal read-only configuration:

```json
{
  "version": 1,
  "targets": {
    "docs": {
      "alias": "docs-prod",
      "label": "Production docs",
      "remoteCwd": "/srv/docs",
      "authority": "read-only",
      "enabled": true
    }
  }
}
```

Target IDs match `[a-z][a-z0-9-]{0,31}` and are the only values accepted by
`/ssh connect`. Aliases use a conservative letters/digits/dot/underscore/hyphen
subset and cannot begin with `-`. Remote cwd is an absolute normalized POSIX
path. Authority defaults to `read-only`; opt into `read-write` only after
reviewing the remote account and cwd.

`config.schema.json` documents the complete schema. The parser additionally
rejects duplicate/unknown keys, invalid UTF-8, oversized files, symlink final
files, files not owned by the current user, group/world-writable files,
controls/NUL, duplicate IDs/aliases, and out-of-range resource settings.

### Digest-pinned trusted project configuration

Project files are never discovered on their own. A user file may explicitly
name an absolute file below the active workspace's `.ygg/` directory and pin its
exact bytes:

```json
{
  "version": 1,
  "targets": {},
  "trustedProjects": [
    {
      "path": "/absolute/workspace/.ygg/ssh.json",
      "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }
  ]
}
```

Generate the digest only after review (for example, `shasum -a 256 FILE`). An
edit invalidates the pin and leaves the extension inspectably degraded. Project
files may contain only `version` and `targets`; they cannot include another file
or override a user target/alias.

### Default bounds

| Resource | Default | Package maximum |
| --- | ---: | ---: |
| live owner sessions | 8 | 16 |
| configured targets | 32 | 32 |
| connect timeout | 10 s | 30 s |
| operation timeout | 30 s | 120 s |
| retained output (stdout + stderr) | 128 KiB | 128 KiB |
| one remote file read/write | 128 KiB | 128 KiB |
| command argv items | 64 | 128 |
| command argv bytes | 32 KiB | 64 KiB |
| retained activities | 64 | 128 |
| health interval | 2 s | 60 s |
| local termination grace | 250 ms | 2 s |

API `0.2` message, queue, structured-result, and presentation bounds apply in
addition.

## Workflow and tools

Target selection is an explicit user/front-end action:

```text
/ssh status
/ssh list
/ssh snapshot
/ssh show <target>
/ssh connect <target>
/ssh retry <target>
/ssh disconnect <target>
```

A connect action creates the owner-fenced OpenSSH ControlMaster immediately when
the command context has its resource owner. If an older host supplies only a
session ID, selection is recorded but no process starts until an SSH tool call
supplies the complete owner fence. Connection setup is the only replay-safe
setup; a degraded connection never reconnects without `/ssh retry`.

The model-callable tools intentionally have no target field:

- `ssh_status {}` — bounded selected-session state and configured target IDs;
- `ssh_read {path, offset?, max_bytes?, timeout_ms?}` — bounded read-only bytes;
- `ssh_list {path?, timeout_ms?}` — bounded read-only directory listing
  (relative subpath, or the configured cwd when omitted); a missing path fails
  with a structured `remote_not_found` error, as does a missing `ssh_read`
  path;
- `ssh_exec {argv, timeout_ms?}` — bounded direct argv, always classified as a
  mutation and freshly approved; and
- `ssh_write {path, data, encoding?, overwrite?, timeout_ms?}` — bounded
  UTF-8/base64 input, written through a temporary file and remote rename after
  fresh approval.

When at least one connection is active, the extension also contributes one
bounded prompt-context block (via API `0.2` `context/collect`) naming the live
target, cwd, and authority so models operate on the remote workspace without
re-deriving session state. The contribution is process-scoped and disappears
when no connection is active.

OpenSSH ultimately passes a remote command to the remote login shell. The
adapter shell-quotes each argv item and its package-authored file scripts, but a
program named by `ssh_exec` still has the full remote account authority. A user
can explicitly request a shell in argv; that does not lower the mutation class
or approval requirement.

Remote output is capped, tagged `untrusted`, and bracketed as untrusted remote
data in model-visible text. Invalid UTF-8, ANSI/control bytes, and binary file
content are represented as base64. Connection banners and raw OpenSSH stderr
are drained but never promoted into results, presentation, or diagnostics.

## Cancellation, ambiguity, health, and cleanup

Every operation runs in its own local process group. Cancellation and timeout
cooperatively observe the API `0.2` request token, terminate the group, wait a
bounded grace, and force-kill survivors. Master/control processes are also
owned, health-checked, and cleaned on disconnect, owner settlement, extension
shutdown, stdin loss, or Ygg's final outer process-group fence.

Cancellation is not rollback. If a mutation times out, is cancelled, loses its
SSH channel, or exits with OpenSSH's transport status `255`, its remote outcome
is marked **ambiguous**. The master is closed, the session becomes degraded,
and neither the command nor connection is replayed. `/ssh retry` is an explicit
user recovery action after inspecting the remote system; it starts a new
connection generation and never repeats prior work. Reads can fail/degrade but
are not automatically replayed either.

Diagnostics record only configured alias, opaque owner, connection generation,
class (`connection_setup`, `read`, or `mutation`), outcome, bounded timing, and
exit status. They never record command argv, file paths/content, output,
banners, environment values, agent socket paths, keys, or auth material.

## TUI, Serve, and headless presentation

The extension emits complete monotonic generic `presentation/update` snapshots:

- persistent high-authority status with alias, read-only/read-write mode, remote
  cwd, connection generation, and ready/degraded state;
- bounded `remote` read/mutation activity with alias/class provenance;
- a host-rendered configured-target/session tree and bounded detail; and
- literal connect/disconnect/retry actions routed to the declared `ssh` command.

It ships no TUI widget, ANSI renderer, web JavaScript, terminal proxy, or
SSH-specific frontend state machine. TUI and Serve project the same resident
extension state. Reconnect/resync reads a complete snapshot and cannot connect,
retry, or replay an operation. Host process-generation fencing removes stale
snapshots.

`fixtures/presentation/` covers disconnected, connecting, read-only,
read-write, degraded, read/mutation activity, cancellation, ambiguous
disconnect, explicit reconnect/resync, and stale-generation removal. `/ssh`
provides the bounded text/JSON fallback for narrow, plain, print, and headless
frontends. An unavailable action/confirmation fails closed.

## Offline/local smoke and tests

Configuration validation and bundle installation are offline and never contact
a host. The deterministic package fixture emulates the OpenSSH CLI locally; it
covers existing-agent propagation, allowlisting, cwd/generation ownership,
bounded output/files, cancellation, timeout, ambiguous mutation, explicit
reconnect, health, and descendant cleanup without keys or external services.

```console
cd extensions/ygg-ssh
python3 -m unittest discover -s tests -t . -v
```

For a real local-only smoke test, configure a normal OpenSSH alias that points to
a separately managed localhost SSH server and authenticate it non-interactively,
then use `/ssh connect <configured-id>`. The package does not install or
configure an SSH server, account, key, agent, or known-host entry.
