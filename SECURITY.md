# Security Policy

Ygg is a **trusted local agent**: a coding agent that runs on your machine, as
your operating-system user account, inside that account's existing security
boundary. This document describes what that model does and does not protect, so
that reports and expectations are calibrated to it.

## Security boundary

- Ygg runs locally as the current operating-system user.
- Commands Ygg launches run with the permissions, filesystem access, environment
  access, and network access of the Ygg process and that user.
- The current user's writable files and configuration are **inside the same
  trust boundary** as Ygg itself. Anything that user can already read or write —
  repositories, local configuration, environment variables, shell startup files,
  `AGENTS.md` instructions, installed executables, and (once they exist)
  extensions and skills — is trusted input, not an attack surface Ygg defends.
- Ygg does **not** provide operating-system process isolation. It does not ship
  a Landlock, seccomp, namespace, Seatbelt, container, or virtual-machine
  backend. Users who require containment must run Ygg inside a container, a
  virtual machine, a restricted user account, or another OS-level sandbox.

Because local write access already permits modifying repositories, configuration,
environment, shell files, `AGENTS.md`, extensions, or skills, exercising that
access is **not itself a Ygg vulnerability**. A valid report must demonstrate
that Ygg *granted* access the user did not already have, *bypassed a documented
Ygg boundary*, or *crossed an operating-system privilege boundary*.

## Local file access

Ygg defaults to **trusted-local** file access. Its built-in `read`, `search`,
and `edit` tools accept paths available to the current OS user, including
absolute paths and `~/…`; relative paths are based at the selected workspace.
`exec` likewise accepts local working directories, and spawned processes run
with the current user's filesystem, network, environment, and subprocess
access.

A host may set `allow_external_paths = false` (or
`YGG_ALLOW_EXTERNAL_PATHS=false`) to opt into a workspace-only accidental-path
guard for explicit built-in tool paths and `exec` working directories. That
optional guard rejects absolute paths, `..` components, and external symlink
targets. It is **not** an operating-system security boundary: spawned processes
remain unconfined regardless of this setting.

Process and shell execution are capability settings, not containment. Enabling
either is equivalent to "run any program this user can run."

## Trusted inputs

Ygg must only be used with trusted:

- repositories;
- project instructions (`AGENTS.md` and similar);
- local configuration (`~/.ygg`, environment, shell);
- model providers and endpoints;
- installed executables;
- extensions and skills, once those features exist.

Repository content, source comments, build output, project instructions, and
tool output can all carry instructions that steer a language model — i.e. they
can **prompt-inject** the agent. Ygg does not claim to prevent prompt injection
originating from repository content or other trusted inputs. Treat every input
you point Ygg at as something you already trust to run on your machine.

## Out of scope

Unless a separate, documented Ygg boundary or an OS privilege boundary is
crossed, the following are **outside** Ygg's security model and are not
vulnerabilities:

- local code execution performed by the coding agent (this is its purpose);
- access already available to the current user;
- the absence of process sandboxing / OS isolation;
- risks arising from untrusted repositories;
- prompt injection from repository content, instructions, or tool output;
- malicious model output;
- user-installed malicious extensions, skills, packages, or tools;
- user-approved or user-initiated actions;
- issues that first require the ability to modify trusted local files,
  environment variables, shell configuration, project instructions, or Ygg
  configuration;
- intentionally weakened user configuration;
- local resource exhaustion caused by trusted input;
- public-internet exposure caused by an unsupported deployment.

## In scope

Examples of reports that **may** be valid:

- crossing an operating-system user or privilege boundary;
- bypassing an explicitly configured Ygg boundary (for example, defeating a
  workspace-only path guard on a built-in tool's path argument);
- Ygg itself granting unauthorized local read or write access;
- unauthorized remote access through a service or interface shipped by Ygg;
- exposure of infrastructure credentials controlled by the Ygg project;
- reachable vulnerabilities in dependencies shipped by Ygg, with demonstrated
  impact.

## Reporting a vulnerability

Please report suspected vulnerabilities privately and do not open a public issue
for security-sensitive reports.

A useful report includes:

- a description of the issue and its impact;
- reproduction steps or a proof of concept;
- the affected package, version, or commit;
- any known mitigations.

> **Maintainer action item — configure a private reporting channel.**
> This repository does not yet have a verified private security contact. Before
> publishing, a maintainer must enable GitHub's private vulnerability reporting
> (Security → *Report a vulnerability*) for this repository and/or add a
> monitored security email address here. Do not substitute an unverified
> address.
