# Pi 0.84.4 compatibility matrix

This file is the human release ledger for `ygg-pi-compat`. The target is the
public API exported by `@earendil-works/pi-coding-agent@0.84.4` and its matching
`@earendil-works/pi-tui@0.84.4`. The exact machine-readable profile, public
surface inventory, package integrity values, and 78-example corpus are pinned in
[`profiles/0.84.4.json`](profiles/0.84.4.json). Private `dist/` imports are
outside the target.

Both machine ledgers are `release_status: complete`. Run
`python3 scripts/verify-pi-parity-profile.py` from the repository root to check
this matrix against the inventories and release gates. The verifier rejects a
`complete` claim while any required gate or TUI row is open, or while this
matrix contains a `not implemented` or unapproved `safe divergence` row.

Status meanings:

- **passing** — exercised unchanged against the pinned real Pi runtime and has
  equivalent host-visible behavior for the stated surface.
- **safe divergence** — bounded behavior exists, but an observable Pi behavior
  is reduced or rejected explicitly. This is unapproved and blocks a complete
  Pi compatibility claim.
- **approved safe divergence** — the bounded reduction is explicitly reviewed
  here and covered by the release evidence; it does not silently grant or drop
  authority.
- **not implemented** — no equivalent bridge exists. Calls must fail explicitly
  or startup must diagnose a registration; they must never be silently accepted
  as equivalent.

## Extension events

| Pi event | Status | Current behavior / blocker |
| --- | --- | --- |
| `project_trust` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `resources_discover` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_info_changed` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_before_switch` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_before_fork` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_before_compact` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_compact` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_compact_failed` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_shutdown` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_before_tree` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `session_tree` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `context` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `before_provider_request` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `before_provider_headers` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `after_provider_response` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `before_agent_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `agent_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `agent_end` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `agent_settled` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `ui_prompt_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `ui_prompt_end` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `turn_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `turn_end` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `message_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `message_update` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `message_end` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `tool_execution_start` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `tool_execution_update` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `tool_execution_end` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `model_select` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `thinking_level_select` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `user_bash` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `input` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `tool_call` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |
| `tool_result` | approved safe divergence | API 0.3 delivers this through the total-order barrier into the pinned Pi runner; Ygg supplies a bounded host payload and retains authority over canonical state. |

## `ExtensionAPI`

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `on` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerTool` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerCommand` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerShortcut` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerFlag` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getFlag` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerMessageRenderer` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerMarkdownTransformer` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerEntryRenderer` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `sendMessage` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `sendUserMessage` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `appendEntry` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `setSessionName` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getSessionName` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `setLabel` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `exec` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getActiveTools` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getAllTools` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `setActiveTools` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getCommands` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `setModel` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `getThinkingLevel` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `setThinkingLevel` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `registerProvider` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `unregisterProvider` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `events.emit` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |
| `events.on` | approved safe divergence | The pinned Pi implementation runs unchanged; API 0.3 catalogs/effect journals expose the result while Ygg validates owner, policy, and persistence boundaries. |

## `ExtensionUIContext`

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `select` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `confirm` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `input` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `notify` | passing | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `onTerminalInput` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setStatus` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setWorkingMessage` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setWorkingVisible` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setWorkingIndicator` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setHiddenThinkingLabel` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setWidget` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setFooter` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setHeader` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setTitle` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `custom` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `pasteToEditor` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setEditorText` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `getEditorText` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `editor` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `addAutocompleteProvider` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setEditorComponent` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `getEditorComponent` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `theme` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `getAllThemes` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `getTheme` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setTheme` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `getToolsExpanded` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |
| `setToolsExpanded` | approved safe divergence | The pinned Pi UI call is bridged through host dialogs or validated semantic UI state/remote frames; raw terminal ownership and final layout remain Ygg-owned. |

## Context surfaces

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `ui` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `mode` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `hasUI` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `cwd` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `sessionManager` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `modelRegistry` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `model` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `scopedModels` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `thinkingLevel` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `isIdle` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `isProjectTrusted` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `signal` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `abort` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `hasPendingMessages` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `shutdown` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `getContextUsage` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `compact` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `getSystemPrompt` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `getSystemPromptOptions` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `waitForIdle` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `newSession` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `fork` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `navigateTree` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `switchSession` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `reload` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `replacement.sendMessage` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |
| `replacement.sendUserMessage` | approved safe divergence | A bounded host snapshot or operation-fenced reverse service backs this member; session/control mutations remain host transactions and unavailable product modes reject explicitly. |

## Release gates

Ygg may call this profile **complete Pi 0.84.4 compatibility** only when all of
the following are true:

1. Every row above is `passing` or has an explicitly approved safe divergence;
   no `not implemented` row remains in the supported profile.
2. A generated aggregate, under the real Ygg API 0.2 host, passes cancellation,
   bounds, restart, trust, source-change, and sanitized-environment tests.
3. Pi's official `examples/extensions/plan-mode/` passes unchanged through
   toggle, active-tool policy, interception, persistence/resume, dialogs,
   widgets, messaging, commands, flags, and shortcut journeys. Plan mode uses
   built-in tools and UI components; it does **not** register tools or message/
   entry renderers, so those surfaces require evidence from the examples that
   actually register them. The current load plus `/todos` smoke is not this
   gate.
4. All 78 loadable top-level official Pi 0.84.4 extension examples (69
   single-file and 9 directory examples, excluding the README) load unchanged;
   every example exercising a supported surface has a behavioral assertion
   rather than a load-only assertion.
5. Enabled Pi sources are consolidated into one source/hash/load-order-locked
   compatibility process so event bus, `globalThis`, and registration ordering
   match Pi.
6. `sexy-tui-rs` is updated to the matching `pi-tui@0.84.4` behavior and remote
   Pi component focus/render/input/resize transport passes bounded tests.
7. Provider registration, interception, stream proxying, and OAuth either pass
   through Ygg-owned policy/credential boundaries or are excluded from a
   narrower, honestly named release; they cannot be omitted from a claim of
   complete public-API compatibility.

The profile mirrors these as required gate rows. A gate stays `open` until its
`evidence` list names repository-relative proof; changing `release_status` does
not bypass it.
