# Ygg sessions

Ygg stores each conversation as bounded, append-only JSONL under the configured
session directory, namespaced by workspace. The JSONL file is the durable
conversation and branch history. Readable names and tags live in small sidecars
under the workspace store's `.metadata/` directory so older binaries can still
open the conversation unchanged.

## Commands

```console
ygg sessions list
ygg sessions list --query review
ygg sessions inspect <id>
ygg sessions rename <id> "parser cleanup"
ygg sessions tag <id> rust local-model
ygg sessions export <id>
ygg sessions export <id> --output ./handoff.ygg-session.json
ygg sessions delete <id>
ygg sessions repair <id>
```

`list` searches IDs, names, derived titles, tags, internal JSONL paths, and
dates encoded in IDs. Lists and searches are intentionally scoped to the
selected workspace's store; Ygg does not maintain a cross-workspace index.
Modified times are shown as readable relative ages. `inspect` validates the
file read-only and reports its derived active-branch title, size, entry count,
head, checkpoint and usage totals, plus branch roots and leaves. Both `inspect`
and the TUI's `/tree` render the complete parent-linked history as a stable
connector tree. `+` traces the selected branch and `*` marks its exact durable
head, so abandoned forks remain visible without looking like active context.
`/checkout <entry-id>` creates a new branch by moving the durable head without
deleting ancestry.

Interactive shortcuts operate on the current session:

- `/name [name]` shows or changes its readable name.
- `/sessions` opens the local session list.
- `/export [path]` writes a redacted portable export.

`delete` is recoverable: it moves the JSONL and metadata into `.trash/` rather
than unlinking them. `repair` is deliberately narrow. It first writes an
owner-private backup, then removes only an interrupted final append. Corruption
in any completed record is diagnosed and never rewritten automatically.

## JSONL schema

Every physical line is one JSON object with a `type` discriminator. There are
four top-level record types:

| Record | Purpose |
| --- | --- |
| `entry` | An immutable parent-linked conversation or state entry. |
| `head` | Selects the active branch head and records cumulative cost. |
| `checkpoint` | Marks a completed prompt and exact restorable head. |
| `usage` | Stores provider/model/token/cost accounting for one operation. |

An entry has this stable envelope:

```json
{
  "type": "entry",
  "id": "entry-id",
  "parent": "previous-entry-id",
  "metadata": {
    "prompt_model": "local-model-id",
    "prompt_model_source": "local",
    "prompt_color": "#5a36d6"
  },
  "value": {
    "type": "message"
  }
}
```

`parent` is `null` for a root. Entry value types are `message`, `compaction`,
`config`, `prompt_template_selected`, `skill_activated`,
`skill_resource_read`, and `skill_deactivated`. Prompt-template selection keeps
the chosen name and content hash outside model-visible context. Skill entries
make explicit activation and lazily loaded resources resumable; compaction
records snapshot the active skill state and cumulative Pi-compatible
`details.readFiles`/`details.modifiedFiles` lists. Those detail fields default
to empty lists when older session records omit them.

`metadata.prompt_color` is the normalized sRGB prompt-gutter colour assigned
at the original user append. It is inert presentation data and is never sent
to a provider. Once written it is authoritative: resume, checkout, branching,
compaction, model switches, and theme reloads do not recalculate it. Legacy
prompts without the field may derive a deterministic display fallback from
their own historical `prompt_model`; the currently selected model is never
used to recolour history.

A head record is the only branch-selection mutation:

```json
{
  "type": "head",
  "id": "entry-id",
  "total_cost_microdollars": 0,
  "total_cost_picodollars_remainder": 0
}
```

Entries are never rewritten when branching or compacting. Context is rebuilt
by walking parents from the selected head and applying any compaction boundary.
Completed JSONL records are strict UTF-8 and strict JSON. Only a final
unterminated record is considered a recoverable torn append. Reads are bounded
by both file bytes and record count.

## Portable export and redaction

Export writes an owner-private `ygg-session-export` version 1 JSON package with
source identity, readable metadata, and validated records. Existing paths are
not replaced without `--force`.

Redaction is on by default. It replaces values under credential-like keys and
also scans string content in prompts, tool arguments, and tool results. The
bounded deterministic scanner covers authorization and cookie headers, common
API-token prefixes, credential assignments and URL query parameters, URL
userinfo, private-key blocks, and JSON objects or arrays serialized inside
strings. It preserves surrounding prose and UTF-8, and reports each replaced
value or credential fragment. This is a safety filter, not a proof that
arbitrary prose contains no secret. Use `--include-secrets` only for a trusted
destination; Ygg prints an explicit warning when raw values are requested.

HTML export, hosted viewers, and cloud sharing are intentionally outside this
local-first session boundary.
