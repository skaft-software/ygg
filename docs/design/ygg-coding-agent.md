# `ygg-coding-agent` design

**Status:** Current implementation contract.

`ygg-coding-agent` is the product layer over `ygg-ai` and `ygg-agent`. It owns
configuration, provider authentication, session discovery, compaction policy,
skill discovery, and the interactive/plain/print frontends. It does not
reimplement the model/tool loop.

The shared customization discovery, trust, limits, diagnostics, precedence,
and reload contract is documented in [`../resources.md`](../resources.md).

## Build and dependency boundary

The workspace MSRV is Rust 1.86. `sexy-tui-rs` is vendored as
`crates/sexy-tui-rs`; builds must not depend on a sibling checkout. Its import
provenance is recorded in `crates/sexy-tui-rs/VENDORED.md`.

## Startup and resume

Startup resolves the persistent session before final model selection:

1. Select a new, latest, named, or interactively picked session.
2. For an existing session, walk its active parent chain and recover the newest
   model and reasoning values from `EntryValue::Config` records.
3. Explicit `--model` and `--reasoning` flags override recovered values.
   Project/global defaults apply only when the session has no corresponding
   value.
4. Resolve and normalize the model/reasoning pair.
5. Append the effective configuration as provenance before constructing
   `Agent`, except when an existing session already ends in the same marker.

Runtime `/resume` and branch checkout use the same restoration behavior.
Interactive resume hydrates only a bounded active-branch tail for first paint;
the complete branch is materialized when the user first navigates beyond that
tail or selects the complete semantic transcript.

## System prompt

The stable, model-agnostic base contract gives both local and cloud models an
explicit completion trajectory: honor answer/investigate/review/plan/implement
mode; use tools rather than guess; inspect before editing; continue until done
or concretely blocked; preserve unrelated work; make the smallest complete
change; verify the diff and relevant checks; and report concise observed
results with `path:line` references. It forbids commits unless requested and
makes clear that supplied tool schemas are authoritative.

The environment block truthfully distinguishes the workspace root from the
invocation directory. Relative tool paths and the default `bash` working
directory resolve from the workspace root. Enabled core-tool names are listed,
while the contract acknowledges extension and skill tools supplied alongside
them. Behavioral changes require regression tests rather than model-specific
prompt tuning.

Global and trusted workspace `AGENTS.md` files retain root-to-leaf precedence
and are wrapped in path-labelled `<project_instructions>` blocks. Active skill
instructions use labelled blocks with stable IDs and hashes.

## Compaction and handoff summaries

The product pre-request gate and `ygg-agent` overflow recovery share one
Pi-compatible summarization implementation. Conversation messages are first
serialized inside `<conversation>` tags so the model cannot mistake them for a
live turn. Initial and iterative summaries use Pi's exact structured Markdown
contracts; iterative calls provide the prior checkpoint in
`<previous-summary>` tags. Branch-handoff helpers use Pi's corresponding branch
prompt and preamble.

File tracking is deterministic host behavior, not model output. Successful or
failed assistant calls to `read`, `write`, and `edit` contribute paths;
modified paths supersede read-only paths; and deduplicated sorted lists are
appended as `<read-files>` and `<modified-files>` blocks. The cumulative
`readFiles`/`modifiedFiles` details are persisted on compaction entries so later
summaries retain them. Legacy entries deserialize with empty details.

## Agent construction and tools

Every build or idle-boundary rebuild creates one `ExtensionHost` and registers,
in order:

- Core tools: `read`, `edit`, `write`, `bash`, then opt-in `search`. The default
  surface omits `search` because `bash` already provides `rg`/`find`/`ls`.
- Skill tools: `search_skills`, `load_skill`, `read_skill_resource`.

Context budgeting reserves the serialized schemas from that exact host rather
than reproducing a hard-coded subset. A consuming rebuild drops the old Agent
before reopening its session, so only one append handle owns a session file.

## Skills

Skills are discovered from user, workspace, and explicit CLI directories with
explicit paths taking highest precedence. Workspace skills require workspace
trust. Model-visible activation is explicit:

1. `search_skills` returns metadata.
2. `load_skill` verifies trust and required registered/enabled tools, snapshots
   the instructions and content hash, persists `SkillActivated`, and returns
   only compact activation metadata.
3. `read_skill_resource` requires a matching active activation, reloads
   `SKILL.md`, rejects a changed instructions hash, permits only text under
   `references/` or `templates/`, and persists the resource snapshot.

Active instructions are appended once in labelled system-prompt blocks rather
than duplicated in both the prompt and `load_skill` result. Activation/resource
state survives compaction through snapshots in the compaction entry.

## Prompt templates

Prompt templates are discovered from `~/.ygg/prompts/`, trusted
`.ygg/prompts/`, and repeatable `--prompt-template <path>` sources. Explicit
paths have highest precedence. Markdown files use Pi-compatible YAML
frontmatter and `$1`/`$@`/default/slice arguments; Ygg also accepts a small
TOML form and deterministic `{{prompt}}`, `{{workspace}}`, `{{selection}}`,
`{{file:path}}`, and `{{skill:name}}` variables.
Interactive invocations resolve `{{selection}}` from the current semantic
transcript selection without copying to or reading from the system clipboard;
startup and print-mode invocations expand it to an empty string.

Template and included-file reads are bounded, traversal is rejected, and final
expansion is capped before provider submission. Each selection persists its
name and SHA-256 in a non-model-visible session entry. Use `/prompt` to inspect
templates, `/prompt <name> ...` or Pi-compatible `/<name> ...` to invoke one,
and `--prompt <name>` for a startup/print prompt. `--debug-prompt` exposes the
exact deterministic expansion and template hash before provider submission.
Pi `argument-hint` metadata appears in slash autocomplete but is never inserted
into the composed prompt.

## Interactive commands

Commands run immediately when safe or queue to the next idle boundary when they
need Agent/session ownership.

- `/model [id]` — pick or select a model.
- `/cycle-model` — move through available models deterministically.
- `/thinking [level]` — select a capability-gated reasoning level.
- `/theme [name]` — change the current theme.
- `/tool [call-id]` — expand/collapse one tool panel; no ID means latest.
- `/verbose [on|off]` — expand/collapse every tool panel.
- `/compact` — force a compaction attempt.
- `/reload` — reload AGENTS instructions, themes, prompts, and skills.
- `/new`, `/resume [id]` — switch persistent sessions.
- `/tree`, `/checkout <entry-id>` — inspect durable entries and fork from one.
- `/name [name]`, `/sessions`, `/export [path]` — name, find, and safely export
  the current session.
- `/prompt [name] [arguments]` — inspect or expand prompt templates.
- `/skills search|load|reload|off ...` — inspect and explicitly activate skills.
- `/extensions [reload]` — inspect or reload trusted executable extensions.
- `/status`, `/help`, `/quit` — product status and lifecycle controls.

Checkout appends a durable head record. The subsequent consuming rebuild
restores configuration on the selected branch and appends current provenance;
future messages therefore fork without deleting the abandoned branch.

`/skills` also reports bounded discovery and validation diagnostics from the
current reload generation. A malformed manifest, rejected link, or ID mismatch
does not prevent healthy skills from loading and no longer disappears into
startup-only stderr.

## Authentication

Codex OAuth credentials live in an owner-only directory and file. Writes use a
same-directory temporary file, flush and sync it, atomically replace the target,
and sync the directory on Unix. Token-bearing types redact both tokens from
`Debug`; token endpoint failures expose only status and a constrained OAuth
error code, never the raw response body.

## Frontends

Interactive, plain, and print modes share `App`, `Agent`, session persistence,
reasoning normalization, and finish classification. TUI-specific rendering and
terminal ownership are specified in `docs/design/ygg-tui.md`.
