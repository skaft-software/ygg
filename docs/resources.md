# Custom resource discovery

Themes, prompts, skills, and executable extensions share one filesystem
resolver. Resource-specific parsers own their schemas; the resolver owns the
cross-cutting local safety and precedence contract.

## Locations and precedence

| Kind | Global | Trusted project | Explicit option |
| --- | --- | --- | --- |
| Theme | `~/.ygg/themes/*.toml` | `.ygg/themes/*.toml` | `--theme-dir` |
| Prompt | `~/.ygg/prompts/*.{md,toml}` | `.ygg/prompts/*.{md,toml}` | `--prompt-template <file-or-dir>` |
| Skill | `~/.ygg/skills/*/SKILL.md` | `.ygg/skills/*/SKILL.md` | `--skill-dir` |
| Extension | `~/.ygg/extensions/*/extension.toml` | `.ygg/extensions/*/extension.toml` | `--extension-dir` |

Roots are visited global, project, then explicit in option order. An explicit
Pi-compatible prompt source may be one `.md`/`.toml` file or a directory.
Later definitions with the same resource name win, and the shadowed path
remains in the diagnostic snapshot. Scans and result ordering are
deterministic.

Workspace resources are ignored until `--workspace-trusted` is present.
Explicit paths are an intentional user choice for that invocation. Executable
extensions add a second boundary: discovery and workspace trust still do not
launch code. The manifest name must be both enabled and independently trusted.
A project config cannot grant itself executable trust. Bare persistent trust
names apply only to the global extension directory; project and explicit
extensions require an exact absolute `name@.../extension.toml` grant or a
one-invocation `--trust-extension name` decision. The extension directory name
must match the manifest name.

If Ygg cannot resolve an absolute user home directory, global configuration and
global resources are disabled with a diagnostic. It never falls back to the
invocation directory and reclassifies project files as user-owned resources.

## Reads and diagnostics

Resource roots, selected files, and directory entrypoints must be regular,
non-symlink filesystem objects. Parser reads use descriptor-bound no-follow
opens and fixed byte limits:

| Kind | Maximum parser input |
| --- | ---: |
| Theme | 256 KiB |
| Prompt | 512 KiB |
| Skill entrypoint | 256 KiB |
| Extension manifest | 256 KiB |

Prompt expansion, skill resource reads, extension protocol messages, and
session files have their own narrower purpose-specific limits after discovery.
Invalid UTF-8, invalid names, inaccessible roots, rejected links, oversized
files, parser failures, and precedence decisions become inspectable
diagnostics. One broken customization does not prevent the core binary from
starting.

## Reload

Each discovery pass produces an immutable generation snapshot. Consumers build
a complete replacement from the new snapshot and swap only after validation,
so an in-flight prompt never observes half of a reload.

- `/theme reload` reloads the selected theme safely.
- `/skills reload` refreshes the shared prompt/skill resource boundary.
- `/extensions reload` handshakes replacement processes before swapping them.
- `/reload` performs full product resource discovery and rebuilds the active
  customization boundary.
