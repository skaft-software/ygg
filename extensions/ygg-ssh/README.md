# ygg-ssh

`ygg-ssh` is Ygg's API `0.2` **SSH portal** for OpenSSH aliases that the user
already configured and authenticated in their own `~/.ssh/config`.

It registers **zero model tools**. Instead, once a target is connected, it
contributes one small prompt-context block telling the model that it is now
operating through an SSH tunnel on another machine, which alias to use, and
that remote output is untrusted data. The agent then does all real work with
its normal shell tool (`ssh <alias> '<command>'`) — exactly as capable as the
user who authenticated the alias.

```text
/ssh connect <id>  →  verified selection + context block
model's bash       →  system ssh  →  configured alias (full remote work)
```

## Design

- The extension owns no tunnels and no long-lived processes. Connection
  multiplexing is the user's own `~/.ssh/config` decision (`ControlMaster`,
  `ControlPersist`); OpenSSH picks it up automatically.
- The only command the extension ever runs is a bounded connectivity probe:
  `ssh -o BatchMode=yes -o NumberOfPasswordPrompts=0 -o ConnectTimeout=<n> -- <alias> true`.
  It never surfaces banners or stderr.
- A model tool or `/ssh` argument can never supply a host, user, port,
  ProxyJump, identity file, or agent socket. `/ssh connect` accepts only a
  stable target ID from the strict user-owned configuration.
- There are no authority modes: in Ygg default/full-access mode the agent has
  whatever permissions the logged-in remote account has — enforced by OpenSSH
  on the host side, not re-implemented here. Safe mode never starts executable
  extensions at all.
- Remote output is untrusted data from a different host; the context block
  says so explicitly.

## Requirements and installation

- Ygg exactly `0.6.0-dev` (`requires_ygg = "=0.6.0-dev"`)
- Python 3.9 or newer
- a system OpenSSH `ssh` client
- a non-interactive, already authenticated OpenSSH alias

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

Configure and test the alias outside Ygg first:

```sshconfig
# ~/.ssh/config
Host docs-prod
    HostName docs.internal.example
    User deploy
    IdentityAgent ~/.ssh/agent.sock
```

Verify non-interactive auth before connecting:

```console
ssh -o BatchMode=yes docs-prod true
```

Optional but recommended for snappy agent sessions — let OpenSSH reuse one
authenticated connection instead of re-handshaking per command:

```sshconfig
Host *
    ControlMaster auto
    ControlPath ~/.ssh/cm-%r@%h:%p
    ControlPersist 10m
```

## Configuration

The default file is `~/.ygg/ssh.json`. A missing file is valid and inert.
Copy the disabled example and validate without connecting:

```console
mkdir -p ~/.ygg
cp extensions/ygg-ssh/config.example.json ~/.ygg/ssh.json
chmod 600 ~/.ygg/ssh.json
$EDITOR ~/.ygg/ssh.json
extensions/ygg-ssh/ygg-ssh --config ~/.ygg/ssh.json --check-config
```

```json
{
  "version": 1,
  "targets": {
    "docs": {
      "alias": "docs-prod",
      "label": "Production docs",
      "cwd": "/srv/docs",
      "enabled": true
    }
  }
}
```

Target IDs match `[a-z][a-z0-9-]{0,31}` and are the only values accepted by
`/ssh connect`. Aliases use a conservative letters/digits/dot/underscore/hyphen
subset and cannot begin with `-`. `cwd` is an optional absolute POSIX path
hint surfaced to the model. `config.schema.json` documents the full schema;
the parser additionally rejects duplicate/unknown keys, invalid UTF-8,
oversized files, symlinked files, files not owned by the current user,
group/world-writable files, control characters/NUL, duplicate IDs/aliases,
and out-of-range counts.

## Workflow

```text
/ssh status                 inspect configured targets and active sessions
/ssh list                   same listing
/ssh show <target>          one target's detail
/ssh connect <target>       verify BatchMode auth, then activate the portal
/ssh disconnect <target>    deactivate the portal for this session
```

While at least one session is connected, the extension contributes one bounded
prompt-context block naming the live alias and working-directory hint. It
disappears when nothing is connected. The companion skill
(`skills/ssh-remote-work/SKILL.md`) carries deeper remote-work technique —
loaded by the agent only when needed, not taxed every turn.

## Tests

Everything runs offline; the deterministic fixture emulates the ssh binary.

```console
cd extensions/ygg-ssh
python3 -m unittest discover -s tests -t . -v
```
