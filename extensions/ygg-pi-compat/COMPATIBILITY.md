# Pi 0.84.4 compatibility matrix

This file is the human release ledger for `ygg-pi-compat`. The target is the
public API exported by `@earendil-works/pi-coding-agent@0.84.4` and its matching
`@earendil-works/pi-tui@0.84.4`. The exact machine-readable profile, public
surface inventory, package integrity values, and 78-example corpus are pinned in
[`profiles/0.84.4.json`](profiles/0.84.4.json). Private `dist/` imports are
outside the target.

Status meanings:

- **passing** — exercised unchanged against the pinned real Pi runtime and has
  equivalent host-visible behavior for the stated surface.
- **safe divergence** — bounded behavior exists, but an observable Pi behavior
  is reduced or rejected explicitly. This still blocks a claim of complete Pi
  compatibility unless the divergence is separately approved.
- **not implemented** — no equivalent bridge exists. Calls must fail explicitly
  or startup must diagnose a registration; they must never be silently accepted
  as equivalent.

## Extension events

| Pi event | Status | Current behavior / blocker |
| --- | --- | --- |
| `project_trust` | not implemented | Ygg trust remains host-owned. |
| `resources_discover` | not implemented | No resource-discovery return channel. |
| `session_start` | safe divergence | Emitted from `session/started`; Pi session-manager state is unavailable. |
| `session_info_changed` | not implemented | No matching lifecycle input. |
| `session_before_switch` | not implemented | Session replacement remains host-owned. |
| `session_before_fork` | not implemented | Session replacement remains host-owned. |
| `session_before_compact` | not implemented | No cancel/override compaction boundary. |
| `session_compact` | not implemented | No Pi compaction payload. |
| `session_compact_failed` | not implemented | No Pi compaction failure payload. |
| `session_shutdown` | safe divergence | Emitted once from Ygg settlement/shutdown with a synthetic reason. |
| `session_before_tree` | not implemented | No tree-navigation boundary. |
| `session_tree` | not implemented | No Pi tree result. |
| `context` | safe divergence | Additions can become bounded Ygg context; filtering/replacing canonical history is unavailable. |
| `before_provider_request` | not implemented | Provider interception is not bridged. |
| `before_provider_headers` | not implemented | Provider interception is not bridged. |
| `after_provider_response` | not implemented | Provider interception is not bridged. |
| `before_agent_start` | safe divergence | Messages/system text become bounded context suffixes; exact replacement is unavailable. |
| `agent_start` | safe divergence | Emitted from `turn/started` without Pi's full payload. |
| `agent_end` | safe divergence | Emitted after Ygg response settlement with reduced messages. |
| `agent_settled` | safe divergence | Emitted after the synthetic `agent_end`. |
| `ui_prompt_start` | safe divergence | Pi's runner emits it around bridged dialogs; terminal semantics differ. |
| `ui_prompt_end` | safe divergence | Pi's runner emits it around bridged dialogs; terminal semantics differ. |
| `turn_start` | safe divergence | Uses a synthetic turn index/timestamp. |
| `turn_end` | safe divergence | Tool results and message history are reduced. |
| `message_start` | not implemented | Ygg does not expose this stream boundary to extensions. |
| `message_update` | not implemented | Ygg does not expose this stream boundary to extensions. |
| `message_end` | not implemented | Message replacement is unavailable. |
| `tool_execution_start` | safe divergence | Lifecycle is emitted, but arguments may be reduced. |
| `tool_execution_update` | safe divergence | Pi-tool partial updates traverse Pi's runner and bounded Ygg progress; real-Pi conformance is still pending. |
| `tool_execution_end` | safe divergence | Lifecycle is emitted, but host-tool result details are reduced. |
| `model_select` | not implemented | No model-selection event channel. |
| `thinking_level_select` | not implemented | No reasoning-selection event channel. |
| `user_bash` | not implemented | User shell execution is not delegated to Pi. |
| `input` | not implemented | Input transform/handled semantics are unavailable. |
| `tool_call` | safe divergence | Blocking works and Pi-tool in-place arguments are honored; native-tool mutation and `terminate` fail explicitly. |
| `tool_result` | safe divergence | Pi-tool content/details/error/usage transforms work; native-tool transforms fail explicitly. |

## `ExtensionAPI`

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `on` | safe divergence | Only events described above are equivalent. |
| `registerTool` | safe divergence | Initial registration/execution passes the real `hello.ts` smoke; post-initialization live-catalog conformance is still fixture-only. |
| `registerCommand` | safe divergence | Initialization-time commands become native commands with `runtime_commands`; later command mutations are unavailable. |
| `registerShortcut` | not implemented | Registration is diagnosed at startup. |
| `registerFlag` | safe divergence | Pi defaults remain available internally, but Ygg CLI flag discovery/parsing is absent. |
| `getFlag` | safe divergence | Returns Pi's default/runtime-local value; Ygg cannot supply invocation flag values. |
| `registerMessageRenderer` | not implemented | No remote Pi component host yet. |
| `registerMarkdownTransformer` | not implemented | No transcript transformer boundary. |
| `registerEntryRenderer` | not implemented | No remote Pi component host yet. |
| `sendMessage` | not implemented | Root-agent messaging remains host-owned. |
| `sendUserMessage` | not implemented | Root-agent messaging remains host-owned. |
| `appendEntry` | not implemented | No durable owner-bound Pi custom-entry service. |
| `setSessionName` | not implemented | Session mutation remains host-owned. |
| `getSessionName` | safe divergence | Returns Ygg's latest supplied host snapshot. |
| `setLabel` | not implemented | No Pi entry-label mutation service. |
| `exec` | safe divergence | Uses Pi's public subprocess helper inside the explicitly trusted extension process, not Ygg's model-controlled bash tool. |
| `getActiveTools` | safe divergence | Reports the bridge-local Pi tool set, not Ygg's complete active policy. |
| `getAllTools` | safe divergence | Reports bridged Pi tools, not Ygg's complete tool catalog. |
| `setActiveTools` | not implemented | Requires a bounded host-owned tool-policy overlay. |
| `getCommands` | safe divergence | Reports Pi runner commands; the Ygg command catalog is broader. |
| `setModel` | not implemented | Model selection remains host-owned. |
| `getThinkingLevel` | safe divergence | Derived from Ygg's inspectable reasoning snapshot. |
| `setThinkingLevel` | not implemented | Reasoning mutation remains host-owned. |
| `registerProvider` | not implemented | Provider registration/stream/OAuth proxying is a final release gate. |
| `unregisterProvider` | not implemented | Provider mutation remains host-owned. |
| `events.emit` | safe divergence | Shared only among sources loaded into the same bridge process. |
| `events.on` | safe divergence | Shared only among sources loaded into the same bridge process. |

## `ExtensionUIContext`

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `select` | safe divergence | Uses bounded text input rather than a Pi selector component. |
| `confirm` | safe divergence | Uses Ygg's confirmation service. |
| `input` | safe divergence | Uses Ygg's bounded single-line input service. |
| `notify` | passing | Becomes a declared Ygg notification and never writes protocol stdout. |
| `onTerminalInput` | not implemented | Raw terminal ownership stays in Ygg. |
| `setStatus` | safe divergence | Becomes one semantic status contribution; Pi's keyed composition is reduced. |
| `setWorkingMessage` | not implemented | No owner-bound working-label contribution. |
| `setWorkingVisible` | not implemented | No owner-bound loader visibility control. |
| `setWorkingIndicator` | not implemented | No bounded custom-frame transport. |
| `setHiddenThinkingLabel` | not implemented | No matching semantic surface. |
| `setWidget` | not implemented | Remote rendered components are not implemented. |
| `setFooter` | not implemented | Remote rendered components are not implemented. |
| `setHeader` | not implemented | Remote rendered components are not implemented. |
| `setTitle` | not implemented | Terminal title ownership stays in Ygg. |
| `custom` | not implemented | Remote focused components/overlays are not implemented. |
| `pasteToEditor` | not implemented | Composer mutation remains host-owned. |
| `setEditorText` | not implemented | Composer mutation remains host-owned. |
| `getEditorText` | not implemented | Composer state is not exposed. |
| `editor` | not implemented | Multiline editor handoff is not implemented. |
| `addAutocompleteProvider` | not implemented | Autocomplete remains host-owned. |
| `setEditorComponent` | not implemented | Remote editor components are not implemented. |
| `getEditorComponent` | not implemented | Remote editor components are not implemented. |
| `theme` | safe divergence | Text/style helpers preserve text but intentionally strip Pi styling. |
| `getAllThemes` | not implemented | Pi themes are not Ygg themes. |
| `getTheme` | not implemented | Pi themes are not Ygg themes. |
| `setTheme` | not implemented | Theme selection remains host-owned. |
| `getToolsExpanded` | not implemented | Transcript disclosure state is not exposed. |
| `setToolsExpanded` | not implemented | Transcript disclosure state remains host-owned. |

## Context surfaces

| Public member | Status | Current behavior / blocker |
| --- | --- | --- |
| `ui` | safe divergence | See the UI matrix above. |
| `mode` | safe divergence | Always Pi RPC mode. |
| `hasUI` | safe divergence | True for bridged dialogs, not full Pi TUI availability. |
| `cwd` | safe divergence | Uses the absolute Ygg workspace; dedicated real-Pi conformance is pending. |
| `sessionManager` | not implemented | Durable Pi entries/tree state are not exposed. |
| `modelRegistry` | not implemented | Provider credentials/catalog stay in Ygg. |
| `model` | not implemented | A Pi `Model` object cannot be reconstructed from Ygg's model id. |
| `scopedModels` | not implemented | Returned as an empty snapshot. |
| `thinkingLevel` | safe divergence | Derived from Ygg's reasoning snapshot. |
| `isIdle` | safe divergence | Tracks bridged turn lifecycle only. |
| `isProjectTrusted` | safe divergence | Conservatively returns false; Ygg trust is not projected as Pi project trust. |
| `signal` | safe divergence | Bound to Ygg request cancellation; dedicated real-Pi conformance is pending. |
| `abort` | safe divergence | Cancels the active bridged request; dedicated real-Pi conformance is pending. |
| `hasPendingMessages` | not implemented | Fails explicitly. |
| `shutdown` | not implemented | Extension code cannot terminate the Ygg host. |
| `getContextUsage` | safe divergence | Returns unknown because no bounded usage snapshot is supplied. |
| `compact` | not implemented | Compaction remains host-owned. |
| `getSystemPrompt` | not implemented | Exact effective prompt disclosure is unavailable. |
| `getSystemPromptOptions` | safe divergence | Command context receives only canonical `cwd`. |
| `waitForIdle` | safe divergence | Command dispatch is treated as idle and returns immediately. |
| `newSession` | not implemented | Fails explicitly. |
| `fork` | not implemented | Fails explicitly. |
| `navigateTree` | not implemented | Fails explicitly. |
| `switchSession` | not implemented | Fails explicitly. |
| `reload` | not implemented | Fails explicitly. |
| replacement-context `sendMessage` | not implemented | Session replacement/messaging are unavailable. |
| replacement-context `sendUserMessage` | not implemented | Session replacement/messaging are unavailable. |

## Release gates

Ygg may call this profile **complete Pi 0.84.4 compatibility** only when all of
the following are true:

1. Every row above is `passing` or has an explicitly approved safe divergence;
   no `not implemented` row remains in the supported profile.
2. A generated link, under the real Ygg API 0.2 host, passes cancellation,
   bounds, restart, trust, source-change, and sanitized-environment tests.
3. Pi's official `examples/extensions/plan-mode/` passes unchanged through
   toggle, tool policy, interception, persistence/resume, dialogs, widgets,
   messaging, commands, flags, and shortcut journeys. The current load plus
   `/todos` smoke is not this gate.
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
