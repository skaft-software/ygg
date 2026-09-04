# Pi 0.84.4 compatibility ledger

This is the human view of the canonical machine-readable [0.84.4 ledger](profiles/0.84.4.ledger.json). It targets the public API exported by `@earendil-works/pi-coding-agent@0.84.4` and `@earendil-works/pi-tui@0.84.4`; private `dist/` imports are outside the target.

## Claim and status vocabulary

**Current claim:** `dogfood_conformance`. The executable fixture suite proves declared bridge behavior and explicit safe divergences. It does **not** claim byte-for-byte Pi TUI, provider, OAuth, or full real-runtime equivalence.

- **passing** — the declared host-visible bridge behavior is exercised.
- **safe divergence** — behavior is reduced or rejected visibly, with the named dogfood decision below; no call is silently accepted as equivalent.
- **known dogfood bug** — reserved for a bounded, named defect with the same release decision requirement.

All non-passing rows below use decision `pi-0.84.4-dogfood-explicit-safe-divergence`: current dogfood branch only; release approval is required before a broader equivalence claim.

## Executable inventory

`python3 extensions/ygg-pi-compat/conformance.py --check --json` validates the 118 public-surface rows, all 78 official extension entries (69 files and 9 directories), all 33 Pi TUI audit rows, the six plan-mode journeys, fixture links, and the raw-byte profile integrity sidecar.

`--check` reports `real_runtime: not_supplied` unless a separate full gate is run. A developer smoke with `YGG_PI_REAL_PACKAGE` is useful diagnosis, not integrity evidence.

### Public extension surface

#### Extension events

| Pi surface | Status | Fixture | Declared behavior |
| --- | --- | --- | --- |
| `project_trust` | safe divergence | `events:project_trust` | Event registration is diagnosed explicitly because project_trust is not emitted by the bounded bridge. |
| `resources_discover` | safe divergence | `events:resources_discover` | Event registration is diagnosed explicitly because resources_discover is not emitted by the bounded bridge. |
| `session_start` | safe divergence | `events:session_start` | Emitted from session/started with a host-derived reason. |
| `session_info_changed` | safe divergence | `events:session_info_changed` | Event registration is diagnosed explicitly because session_info_changed is not emitted by the bounded bridge. |
| `session_before_switch` | safe divergence | `events:session_before_switch` | Event registration is diagnosed explicitly because session_before_switch is not emitted by the bounded bridge. |
| `session_before_fork` | safe divergence | `events:session_before_fork` | Event registration is diagnosed explicitly because session_before_fork is not emitted by the bounded bridge. |
| `session_before_compact` | safe divergence | `events:session_before_compact` | Event registration is diagnosed explicitly because session_before_compact is not emitted by the bounded bridge. |
| `session_compact` | safe divergence | `events:session_compact` | Event registration is diagnosed explicitly because session_compact is not emitted by the bounded bridge. |
| `session_compact_failed` | safe divergence | `events:session_compact_failed` | Event registration is diagnosed explicitly because session_compact_failed is not emitted by the bounded bridge. |
| `session_shutdown` | safe divergence | `events:session_shutdown` | Emitted once from settlement or shutdown with a synthetic reason. |
| `session_before_tree` | safe divergence | `events:session_before_tree` | Event registration is diagnosed explicitly because session_before_tree is not emitted by the bounded bridge. |
| `session_tree` | safe divergence | `events:session_tree` | Event registration is diagnosed explicitly because session_tree is not emitted by the bounded bridge. |
| `context` | safe divergence | `events:context` | Bounded additions become Ygg context; canonical history replacement remains host-owned. |
| `before_provider_request` | safe divergence | `events:before_provider_request` | Event registration is diagnosed explicitly because before_provider_request is not emitted by the bounded bridge. |
| `before_provider_headers` | safe divergence | `events:before_provider_headers` | Event registration is diagnosed explicitly because before_provider_headers is not emitted by the bounded bridge. |
| `after_provider_response` | safe divergence | `events:after_provider_response` | Event registration is diagnosed explicitly because after_provider_response is not emitted by the bounded bridge. |
| `before_agent_start` | safe divergence | `events:before_agent_start` | Bounded system/message additions become host context suffixes. |
| `agent_start` | safe divergence | `events:agent_start` | Emitted from turn/started with the bridge lifecycle payload. |
| `agent_end` | safe divergence | `events:agent_end` | Emitted after response settlement with reduced messages. |
| `agent_settled` | safe divergence | `events:agent_settled` | Emitted after the synthetic agent_end event. |
| `ui_prompt_start` | safe divergence | `events:ui_prompt_start` | Event registration is diagnosed explicitly because ui_prompt_start is not emitted by the bounded bridge. |
| `ui_prompt_end` | safe divergence | `events:ui_prompt_end` | Event registration is diagnosed explicitly because ui_prompt_end is not emitted by the bounded bridge. |
| `turn_start` | safe divergence | `events:turn_start` | Emitted with a synthetic turn index and timestamp. |
| `turn_end` | safe divergence | `events:turn_end` | Emitted with reduced tool-result and message history. |
| `message_start` | safe divergence | `events:message_start` | Event registration is diagnosed explicitly because message_start is not emitted by the bounded bridge. |
| `message_update` | safe divergence | `events:message_update` | Event registration is diagnosed explicitly because message_update is not emitted by the bounded bridge. |
| `message_end` | safe divergence | `events:message_end` | Event registration is diagnosed explicitly because message_end is not emitted by the bounded bridge. |
| `tool_execution_start` | safe divergence | `events:tool_execution_start` | Emitted from host tool/started lifecycle. |
| `tool_execution_update` | safe divergence | `events:tool_execution_update` | Pi partial results route through bounded Ygg progress. |
| `tool_execution_end` | safe divergence | `events:tool_execution_end` | Emitted from host tool/settled lifecycle with reduced result details. |
| `model_select` | safe divergence | `events:model_select` | Event registration is diagnosed explicitly because model_select is not emitted by the bounded bridge. |
| `thinking_level_select` | safe divergence | `events:thinking_level_select` | Event registration is diagnosed explicitly because thinking_level_select is not emitted by the bounded bridge. |
| `user_bash` | safe divergence | `events:user_bash` | Event registration is diagnosed explicitly because user_bash is not emitted by the bounded bridge. |
| `input` | safe divergence | `events:input` | Event registration is diagnosed explicitly because input is not emitted by the bounded bridge. |
| `tool_call` | safe divergence | `events:tool_call` | Blocks and Pi-tool argument preparation are preserved; unrepresentable mutation fails. |
| `tool_result` | safe divergence | `events:tool_result` | Pi-tool content/details/error/usage transforms are preserved; native mutation fails. |

#### `ExtensionAPI`

| Pi surface | Status | Fixture | Declared behavior |
| --- | --- | --- | --- |
| `on` | safe divergence | `extension_api:on` | Supported events are registered; unavailable events emit a startup diagnostic. |
| `registerTool` | safe divergence | `extension_api:registerTool` | Initial tool registration and execution use the public Pi runner. |
| `registerCommand` | safe divergence | `extension_api:registerCommand` | Initial commands become native Ygg commands when runtime_commands is negotiated. |
| `registerShortcut` | safe divergence | `extension_api:registerShortcut` | Shortcut registration is diagnosed at startup; host key dispatch remains deferred. |
| `registerFlag` | safe divergence | `extension_api:registerFlag` | Pi exposes flags only after loading extension code, while Ygg discovers trusted API `0.3` manifest flags before startup; bridge registration remains diagnosed. |
| `getFlag` | safe divergence | `extension_api:getFlag` | Returns Pi runtime/default values; the API `0.2` bridge cannot receive Ygg's API `0.3` pre-start invocation values. |
| `registerMessageRenderer` | safe divergence | `extension_api:registerMessageRenderer` | Remote component rendering is rejected explicitly. |
| `registerMarkdownTransformer` | safe divergence | `extension_api:registerMarkdownTransformer` | Transcript mutation is rejected explicitly. |
| `registerEntryRenderer` | safe divergence | `extension_api:registerEntryRenderer` | Remote component rendering is rejected explicitly. |
| `sendMessage` | safe divergence | `extension_api:sendMessage` | Root-session delivery is rejected explicitly; host message projection is deferred. |
| `sendUserMessage` | safe divergence | `extension_api:sendUserMessage` | Root-session delivery is rejected explicitly; host message projection is deferred. |
| `appendEntry` | safe divergence | `extension_api:appendEntry` | Durable custom entries are rejected explicitly; session-state projection is deferred. |
| `setSessionName` | safe divergence | `extension_api:setSessionName` | Session-name mutation is rejected explicitly; session-state projection is deferred. |
| `getSessionName` | safe divergence | `extension_api:getSessionName` | Returns the latest host session-name snapshot. |
| `setLabel` | safe divergence | `extension_api:setLabel` | Entry-label mutation is rejected explicitly. |
| `exec` | safe divergence | `extension_api:exec` | Runs only inside the explicitly trusted Pi extension process, never through Ygg bash. |
| `getActiveTools` | safe divergence | `extension_api:getActiveTools` | Reports bridge-local Pi tools rather than the full Ygg policy. |
| `getAllTools` | safe divergence | `extension_api:getAllTools` | Reports bridge-local Pi tool information. |
| `setActiveTools` | safe divergence | `extension_api:setActiveTools` | Active-tool policy mutation is rejected explicitly; host policy projection is deferred. |
| `getCommands` | safe divergence | `extension_api:getCommands` | Reports commands registered by the Pi runner. |
| `setModel` | safe divergence | `extension_api:setModel` | Model mutation remains host-owned and errors explicitly. |
| `getThinkingLevel` | safe divergence | `extension_api:getThinkingLevel` | Derives a read-only level from the host reasoning snapshot. |
| `setThinkingLevel` | safe divergence | `extension_api:setThinkingLevel` | Reasoning mutation remains host-owned and errors explicitly. |
| `registerProvider` | safe divergence | `extension_api:registerProvider` | Provider and OAuth registration are rejected; no credential or network proxy is implicit. |
| `unregisterProvider` | safe divergence | `extension_api:unregisterProvider` | Provider mutation is rejected explicitly. |
| `events.emit` | safe divergence | `extension_api:events.emit` | Event bus is shared only by sources in the same bridge process. |
| `events.on` | safe divergence | `extension_api:events.on` | Event bus is shared only by sources in the same bridge process. |

#### `ExtensionUIContext`

| Pi surface | Status | Fixture | Declared behavior |
| --- | --- | --- | --- |
| `select` | safe divergence | `ui_context:select` | Uses bounded text input rather than a Pi selector component. |
| `confirm` | safe divergence | `ui_context:confirm` | Uses Ygg confirmation requests. |
| `input` | safe divergence | `ui_context:input` | Uses Ygg bounded single-line input requests. |
| `notify` | passing | `ui_context:notify` | Emits a declared Ygg notification without writing protocol stdout. |
| `onTerminalInput` | safe divergence | `ui_context:onTerminalInput` | Raw terminal ownership stays in Ygg and errors explicitly. |
| `setStatus` | safe divergence | `ui_context:setStatus` | Publishes one semantic Ygg status contribution. |
| `setWorkingMessage` | safe divergence | `ui_context:setWorkingMessage` | Working-label ownership stays in Ygg and errors explicitly. |
| `setWorkingVisible` | safe divergence | `ui_context:setWorkingVisible` | Working-loader visibility stays in Ygg and errors explicitly. |
| `setWorkingIndicator` | safe divergence | `ui_context:setWorkingIndicator` | Custom loading frames are rejected explicitly. |
| `setHiddenThinkingLabel` | safe divergence | `ui_context:setHiddenThinkingLabel` | Hidden-thinking label mutation is rejected explicitly. |
| `setWidget` | safe divergence | `ui_context:setWidget` | Widget transport is rejected explicitly; plain-text host projection is deferred. |
| `setFooter` | safe divergence | `ui_context:setFooter` | Remote footer components are rejected explicitly. |
| `setHeader` | safe divergence | `ui_context:setHeader` | Remote header components are rejected explicitly. |
| `setTitle` | safe divergence | `ui_context:setTitle` | Terminal title ownership stays in Ygg and errors explicitly. |
| `custom` | safe divergence | `ui_context:custom` | Focused remote components are rejected explicitly. |
| `pasteToEditor` | safe divergence | `ui_context:pasteToEditor` | Composer mutation is rejected explicitly. |
| `setEditorText` | safe divergence | `ui_context:setEditorText` | Composer mutation is rejected explicitly. |
| `getEditorText` | safe divergence | `ui_context:getEditorText` | Composer state is rejected explicitly. |
| `editor` | safe divergence | `ui_context:editor` | Editor dialog transport is rejected explicitly; the host-owned editor handoff is not a Pi bridge surface. |
| `addAutocompleteProvider` | safe divergence | `ui_context:addAutocompleteProvider` | Autocomplete mutation is rejected explicitly. |
| `setEditorComponent` | safe divergence | `ui_context:setEditorComponent` | Remote editor components are rejected explicitly. |
| `getEditorComponent` | safe divergence | `ui_context:getEditorComponent` | Remote editor components are rejected explicitly. |
| `theme` | safe divergence | `ui_context:theme` | Text/style helpers preserve text while stripping Pi styling. |
| `getAllThemes` | safe divergence | `ui_context:getAllThemes` | Pi theme inventory is rejected explicitly. |
| `getTheme` | safe divergence | `ui_context:getTheme` | Pi theme lookup is rejected explicitly. |
| `setTheme` | safe divergence | `ui_context:setTheme` | Pi theme mutation is rejected explicitly. |
| `getToolsExpanded` | safe divergence | `ui_context:getToolsExpanded` | Transcript disclosure state is rejected explicitly. |
| `setToolsExpanded` | safe divergence | `ui_context:setToolsExpanded` | Transcript disclosure mutation is rejected explicitly. |

#### Context surfaces

| Pi surface | Status | Fixture | Declared behavior |
| --- | --- | --- | --- |
| `ui` | safe divergence | `context:ui` | Provides the bounded bridge UI context. |
| `mode` | safe divergence | `context:mode` | Always Pi RPC mode rather than an interactive Pi TUI. |
| `hasUI` | safe divergence | `context:hasUI` | True only for bridged dialogs, not arbitrary Pi components. |
| `cwd` | safe divergence | `context:cwd` | Uses the canonical Ygg workspace. |
| `sessionManager` | safe divergence | `context:sessionManager` | Session snapshots and mutation are host-owned and rejected explicitly. |
| `modelRegistry` | safe divergence | `context:modelRegistry` | Credential/model registry access is rejected explicitly. |
| `model` | safe divergence | `context:model` | A Pi Model object is not reconstructed from a Ygg model id. |
| `scopedModels` | safe divergence | `context:scopedModels` | Returns an empty read-only snapshot. |
| `thinkingLevel` | safe divergence | `context:thinkingLevel` | Derives from the host reasoning snapshot. |
| `isIdle` | safe divergence | `context:isIdle` | Tracks the bridged turn lifecycle only. |
| `isProjectTrusted` | safe divergence | `context:isProjectTrusted` | Conservatively returns false because Ygg trust is not projected. |
| `signal` | safe divergence | `context:signal` | Binds to the active Ygg request cancellation signal. |
| `abort` | safe divergence | `context:abort` | Cancels the active bridged request. |
| `hasPendingMessages` | safe divergence | `context:hasPendingMessages` | Queue inspection is rejected explicitly. |
| `shutdown` | safe divergence | `context:shutdown` | Extension code cannot terminate the host and errors explicitly. |
| `getContextUsage` | safe divergence | `context:getContextUsage` | Returns unknown without a bounded host usage snapshot. |
| `compact` | safe divergence | `context:compact` | Compaction is host-owned and errors explicitly. |
| `getSystemPrompt` | safe divergence | `context:getSystemPrompt` | Exact effective prompt disclosure is rejected explicitly. |
| `getSystemPromptOptions` | safe divergence | `context:getSystemPromptOptions` | Returns only canonical cwd. |
| `waitForIdle` | safe divergence | `context:waitForIdle` | Command dispatch is treated as idle and returns immediately. |
| `newSession` | safe divergence | `context:newSession` | Session replacement is rejected explicitly. |
| `fork` | safe divergence | `context:fork` | Session replacement is rejected explicitly. |
| `navigateTree` | safe divergence | `context:navigateTree` | Tree mutation is rejected explicitly. |
| `switchSession` | safe divergence | `context:switchSession` | Session replacement is rejected explicitly. |
| `reload` | safe divergence | `context:reload` | Runtime reload is rejected explicitly. |
| `replacement.sendMessage` | safe divergence | `context:replacement.sendMessage` | Replacement-session messaging is rejected explicitly. |
| `replacement.sendUserMessage` | safe divergence | `context:replacement.sendUserMessage` | Replacement-session messaging is rejected explicitly. |

## Official example inventory

| Entry | Kind | Load fixture | Behavioral fixture |
| --- | --- | --- | --- |
| `auto-commit-on-exit.ts` | file | `example-load:auto-commit-on-exit-ts` | `example-registration:auto-commit-on-exit-ts` |
| `bash-spawn-hook.ts` | file | `example-load:bash-spawn-hook-ts` | `example-registration:bash-spawn-hook-ts` |
| `bookmark.ts` | file | `example-load:bookmark-ts` | `example-registration:bookmark-ts` |
| `border-status-editor.ts` | file | `example-load:border-status-editor-ts` | `example-registration:border-status-editor-ts` |
| `built-in-tool-renderer.ts` | file | `example-load:built-in-tool-renderer-ts` | `example-registration:built-in-tool-renderer-ts` |
| `claude-rules.ts` | file | `example-load:claude-rules-ts` | `example-registration:claude-rules-ts` |
| `commands.ts` | file | `example-load:commands-ts` | `example-registration:commands-ts` |
| `confirm-destructive.ts` | file | `example-load:confirm-destructive-ts` | `example-registration:confirm-destructive-ts` |
| `custom-compaction.ts` | file | `example-load:custom-compaction-ts` | `example-registration:custom-compaction-ts` |
| `custom-footer.ts` | file | `example-load:custom-footer-ts` | `example-registration:custom-footer-ts` |
| `custom-header.ts` | file | `example-load:custom-header-ts` | `example-registration:custom-header-ts` |
| `custom-provider-anthropic` | directory | `example-load:custom-provider-anthropic` | `example-registration:custom-provider-anthropic` |
| `custom-provider-gitlab-duo` | directory | `example-load:custom-provider-gitlab-duo` | `example-registration:custom-provider-gitlab-duo` |
| `dirty-repo-guard.ts` | file | `example-load:dirty-repo-guard-ts` | `example-registration:dirty-repo-guard-ts` |
| `doom-overlay` | directory | `example-load:doom-overlay` | `example-registration:doom-overlay` |
| `dynamic-resources` | directory | `example-load:dynamic-resources` | `example-registration:dynamic-resources` |
| `dynamic-tools.ts` | file | `example-load:dynamic-tools-ts` | `example-registration:dynamic-tools-ts` |
| `entry-renderer.ts` | file | `example-load:entry-renderer-ts` | `example-registration:entry-renderer-ts` |
| `event-bus.ts` | file | `example-load:event-bus-ts` | `example-registration:event-bus-ts` |
| `file-trigger.ts` | file | `example-load:file-trigger-ts` | `example-registration:file-trigger-ts` |
| `git-checkpoint.ts` | file | `example-load:git-checkpoint-ts` | `example-registration:git-checkpoint-ts` |
| `git-merge-and-resolve.ts` | file | `example-load:git-merge-and-resolve-ts` | `example-registration:git-merge-and-resolve-ts` |
| `github-issue-autocomplete.ts` | file | `example-load:github-issue-autocomplete-ts` | `example-registration:github-issue-autocomplete-ts` |
| `gondolin` | directory | `example-load:gondolin` | `example-registration:gondolin` |
| `handoff.ts` | file | `example-load:handoff-ts` | `example-registration:handoff-ts` |
| `hello.ts` | file | `example-load:hello-ts` | `example-registration:hello-ts` |
| `hidden-thinking-label.ts` | file | `example-load:hidden-thinking-label-ts` | `example-registration:hidden-thinking-label-ts` |
| `inline-bash.ts` | file | `example-load:inline-bash-ts` | `example-registration:inline-bash-ts` |
| `input-transform-streaming.ts` | file | `example-load:input-transform-streaming-ts` | `example-registration:input-transform-streaming-ts` |
| `input-transform.ts` | file | `example-load:input-transform-ts` | `example-registration:input-transform-ts` |
| `interactive-shell.ts` | file | `example-load:interactive-shell-ts` | `example-registration:interactive-shell-ts` |
| `kimi-deferred-tools.ts` | file | `example-load:kimi-deferred-tools-ts` | `example-registration:kimi-deferred-tools-ts` |
| `mac-system-theme.ts` | file | `example-load:mac-system-theme-ts` | `example-registration:mac-system-theme-ts` |
| `message-renderer.ts` | file | `example-load:message-renderer-ts` | `example-registration:message-renderer-ts` |
| `minimal-mode.ts` | file | `example-load:minimal-mode-ts` | `example-registration:minimal-mode-ts` |
| `modal-editor.ts` | file | `example-load:modal-editor-ts` | `example-registration:modal-editor-ts` |
| `model-status.ts` | file | `example-load:model-status-ts` | `example-registration:model-status-ts` |
| `notify.ts` | file | `example-load:notify-ts` | `example-registration:notify-ts` |
| `overlay-qa-tests.ts` | file | `example-load:overlay-qa-tests-ts` | `example-registration:overlay-qa-tests-ts` |
| `overlay-test.ts` | file | `example-load:overlay-test-ts` | `example-registration:overlay-test-ts` |
| `permission-gate.ts` | file | `example-load:permission-gate-ts` | `example-registration:permission-gate-ts` |
| `pirate.ts` | file | `example-load:pirate-ts` | `example-registration:pirate-ts` |
| `plan-mode` | directory | `example-load:plan-mode` | `plan-mode:full-journey` |
| `preset.ts` | file | `example-load:preset-ts` | `example-registration:preset-ts` |
| `project-trust.ts` | file | `example-load:project-trust-ts` | `example-registration:project-trust-ts` |
| `prompt-customizer.ts` | file | `example-load:prompt-customizer-ts` | `example-registration:prompt-customizer-ts` |
| `protected-paths.ts` | file | `example-load:protected-paths-ts` | `example-registration:protected-paths-ts` |
| `provider-payload.ts` | file | `example-load:provider-payload-ts` | `example-registration:provider-payload-ts` |
| `qna.ts` | file | `example-load:qna-ts` | `example-registration:qna-ts` |
| `question.ts` | file | `example-load:question-ts` | `example-registration:question-ts` |
| `questionnaire.ts` | file | `example-load:questionnaire-ts` | `example-registration:questionnaire-ts` |
| `rainbow-editor.ts` | file | `example-load:rainbow-editor-ts` | `example-registration:rainbow-editor-ts` |
| `reload-runtime.ts` | file | `example-load:reload-runtime-ts` | `example-registration:reload-runtime-ts` |
| `rpc-demo.ts` | file | `example-load:rpc-demo-ts` | `example-registration:rpc-demo-ts` |
| `sandbox` | directory | `example-load:sandbox` | `example-registration:sandbox` |
| `send-user-message.ts` | file | `example-load:send-user-message-ts` | `example-registration:send-user-message-ts` |
| `session-name.ts` | file | `example-load:session-name-ts` | `example-registration:session-name-ts` |
| `shutdown-command.ts` | file | `example-load:shutdown-command-ts` | `example-registration:shutdown-command-ts` |
| `snake.ts` | file | `example-load:snake-ts` | `example-registration:snake-ts` |
| `space-invaders.ts` | file | `example-load:space-invaders-ts` | `example-registration:space-invaders-ts` |
| `ssh.ts` | file | `example-load:ssh-ts` | `example-registration:ssh-ts` |
| `status-line.ts` | file | `example-load:status-line-ts` | `example-registration:status-line-ts` |
| `structured-output.ts` | file | `example-load:structured-output-ts` | `example-registration:structured-output-ts` |
| `subagent` | directory | `example-load:subagent` | `example-registration:subagent` |
| `summarize.ts` | file | `example-load:summarize-ts` | `example-registration:summarize-ts` |
| `system-prompt-header.ts` | file | `example-load:system-prompt-header-ts` | `example-registration:system-prompt-header-ts` |
| `tic-tac-toe.ts` | file | `example-load:tic-tac-toe-ts` | `example-registration:tic-tac-toe-ts` |
| `timed-confirm.ts` | file | `example-load:timed-confirm-ts` | `example-registration:timed-confirm-ts` |
| `titlebar-spinner.ts` | file | `example-load:titlebar-spinner-ts` | `example-registration:titlebar-spinner-ts` |
| `todo.ts` | file | `example-load:todo-ts` | `example-registration:todo-ts` |
| `tool-override.ts` | file | `example-load:tool-override-ts` | `example-registration:tool-override-ts` |
| `tools.ts` | file | `example-load:tools-ts` | `example-registration:tools-ts` |
| `trigger-compact.ts` | file | `example-load:trigger-compact-ts` | `example-registration:trigger-compact-ts` |
| `truncated-tool.ts` | file | `example-load:truncated-tool-ts` | `example-registration:truncated-tool-ts` |
| `widget-placement.ts` | file | `example-load:widget-placement-ts` | `example-registration:widget-placement-ts` |
| `with-deps` | directory | `example-load:with-deps` | `example-registration:with-deps` |
| `working-indicator.ts` | file | `example-load:working-indicator-ts` | `example-registration:working-indicator-ts` |
| `working-message-test.ts` | file | `example-load:working-message-test-ts` | `example-registration:working-message-test-ts` |

## Pi TUI audit

The Rust TUI is assessed as a semantic boundary rather than an arbitrary Pi component host. Every upstream test row remains visible below as an explicit safe divergence; this is intentionally narrower than a Pi TUI equivalence claim.

| Upstream test | Area | Status | Fixture |
| --- | --- | --- | --- |
| `test/autocomplete.test.ts` | autocomplete | safe divergence | `tui:test-autocomplete-test-ts` |
| `test/bug-regression-isimageline-startswith-bug.test.ts` | terminal-image | safe divergence | `tui:test-bug-regression-isimageline-startswith-bug-test-ts` |
| `test/editor-history-keybindings.test.ts` | editor | safe divergence | `tui:test-editor-history-keybindings-test-ts` |
| `test/editor.test.ts` | editor | safe divergence | `tui:test-editor-test-ts` |
| `test/fuzzy.test.ts` | autocomplete | safe divergence | `tui:test-fuzzy-test-ts` |
| `test/input.test.ts` | input | safe divergence | `tui:test-input-test-ts` |
| `test/keybindings.test.ts` | keybindings | safe divergence | `tui:test-keybindings-test-ts` |
| `test/keys.test.ts` | keys | safe divergence | `tui:test-keys-test-ts` |
| `test/latex.test.ts` | markdown | safe divergence | `tui:test-latex-test-ts` |
| `test/layout.test.ts` | layout | safe divergence | `tui:test-layout-test-ts` |
| `test/markdown.test.ts` | markdown | safe divergence | `tui:test-markdown-test-ts` |
| `test/native-module-path.test.ts` | native-module | safe divergence | `tui:test-native-module-path-test-ts` |
| `test/overlay-non-capturing.test.ts` | overlay | safe divergence | `tui:test-overlay-non-capturing-test-ts` |
| `test/overlay-options.test.ts` | overlay | safe divergence | `tui:test-overlay-options-test-ts` |
| `test/overlay-short-content.test.ts` | overlay | safe divergence | `tui:test-overlay-short-content-test-ts` |
| `test/regression-overlay-cjk-boundary.test.ts` | overlay | safe divergence | `tui:test-regression-overlay-cjk-boundary-test-ts` |
| `test/regression-regional-indicator-width.test.ts` | width | safe divergence | `tui:test-regression-regional-indicator-width-test-ts` |
| `test/select-list.test.ts` | widgets | safe divergence | `tui:test-select-list-test-ts` |
| `test/settings-list.test.ts` | widgets | safe divergence | `tui:test-settings-list-test-ts` |
| `test/stdin-buffer.test.ts` | stdin-buffer | safe divergence | `tui:test-stdin-buffer-test-ts` |
| `test/tab-width.test.ts` | width | safe divergence | `tui:test-tab-width-test-ts` |
| `test/terminal-colors.test.ts` | terminal | safe divergence | `tui:test-terminal-colors-test-ts` |
| `test/terminal-image.test.ts` | terminal-image | safe divergence | `tui:test-terminal-image-test-ts` |
| `test/terminal.test.ts` | terminal | safe divergence | `tui:test-terminal-test-ts` |
| `test/truncate-to-width.test.ts` | width | safe divergence | `tui:test-truncate-to-width-test-ts` |
| `test/truncated-text.test.ts` | widgets | safe divergence | `tui:test-truncated-text-test-ts` |
| `test/tui-alt-screen.test.ts` | tui | safe divergence | `tui:test-tui-alt-screen-test-ts` |
| `test/tui-cell-size-input.test.ts` | tui | safe divergence | `tui:test-tui-cell-size-input-test-ts` |
| `test/tui-overlay-style-leak.test.ts` | overlay | safe divergence | `tui:test-tui-overlay-style-leak-test-ts` |
| `test/tui-render.test.ts` | tui | safe divergence | `tui:test-tui-render-test-ts` |
| `test/tui-shrink.test.ts` | tui | safe divergence | `tui:test-tui-shrink-test-ts` |
| `test/word-navigation.test.ts` | editor | safe divergence | `tui:test-word-navigation-test-ts` |
| `test/wrap-ansi.test.ts` | width | safe divergence | `tui:test-wrap-ansi-test-ts` |

## Plan-mode journeys and deferred bridge remainder

The plan-mode fixture keeps the six upstream journeys visible while separating
existing bounded behavior from the deferred bridge remainder. Its hermetic fake
Pi checks explicit rejection; it is not a substitution for the
integrity-verified unchanged-source full gate.

| Journey | Fixture | Assertion |
| --- | --- | --- |
| plan-toggle-and-policy | `plan-mode:toggle-policy` | Deferred: active-tool policy and widget mutation reject explicitly; status contributions remain available. |
| plan-interception | `plan-mode:interception` | Supported: bounded tool-call interception routes through the declared mutation hook. |
| plan-persistence-resume | `plan-mode:persistence-resume` | Deferred: durable custom entries and session snapshots remain host-owned and reject explicitly. |
| plan-dialogs-and-widgets | `plan-mode:dialogs-widgets` | Deferred: select/input dialogs remain available, while editor and widget transport reject explicitly. |
| plan-messaging | `plan-mode:messaging` | Deferred: root-session message delivery and session-name mutation reject explicitly. |
| plan-commands-flags-shortcuts | `plan-mode:commands-flags-shortcuts` | Deferred: native command catalog remains available; flag projection and shortcut dispatch reject explicitly. |

The current bridge does not consume `host.pi_compat`, emit `pi/*` child methods,
or accept `shortcut/trigger`. Shortcut routing, CLI/flag projection, session
control, root-session messaging, widget/editor transport, and the remaining Pi
bridge surfaces are deliberately deferred. Provider/OAuth registration,
arbitrary component rendering, terminal ownership, model mutation, session-tree
mutation, and shutdown remain explicit safe divergences. The bridge does not
proxy credentials or grant new shell/network authority.

## Integrity-verified unchanged-source full gate

Run only with locally supplied artifacts; the command performs no download. It verifies both npm SRI values, matches the selected coding-agent root and the Pi TUI root Node resolves from it against their verified tarballs, requires a clean checkout at `b79e4cc834970cca69daebffab7df1da7d1e52c4`, fingerprints every source immediately before loading, clears credentials through an allowlisted environment and fresh `HOME`, and uses `unshare --net`.

```sh
python3 extensions/ygg-pi-compat/conformance.py --full --network-isolated \
  --coding-agent-tarball /local/pi-coding-agent-0.84.4.tgz \
  --tui-tarball /local/pi-tui-0.84.4.tgz \
  --pi-package /local/unpacked/pi-coding-agent \
  --source-root /local/pi-source-at-b79e4cc
```

The full gate initializes all 78 unchanged sources through Pi’s public loader. It does not turn extension initialization into permission to use credentials or the network; Linux user/network namespace support is a prerequisite.

## Aggregate publication and API 0.3 evidence seam

A Pi aggregate is published only from a canonical, inert plan. The plan and its
published aggregate lock pin source order, each bounded source fingerprint,
nearby dependency-lock fingerprints, the exact Pi runtime path and package
integrity, bridge/Pi/Ygg versions, and the explicit-enable/explicit-trust mode.
Preflight repeats those checks without importing source; publish repeats
preflight immediately before writing the generated package. The bridge validates
the aggregate/link identity before invoking Pi's loader and rechecks runtime
integrity after loading, so a source or runtime changed between review and start
fails closed rather than becoming a best-effort load.

Published packages include `pi-runtime-evidence.json`, canonicalized with Ygg's
API 0.3 metadata helper. It records static aggregate selection, integrity, and
trust-binding evidence for a future runtime manager. It is not a Pi API 0.3
process protocol, does not provide lazy activation/workspace/reload semantics,
and does not change any status in the matrices above; live bridge protocol
coverage remains API 0.2.

`ygg pi rollback NAME` is intentionally non-destructive: it only moves a
validated generated package out of discovery into a rollback directory. It does
not delete reviewed Pi sources, rewrite an arbitrary extension, or grant/revoke
Ygg extension trust.

## Release policy

A broader Pi-equivalence release needs a separately approved decision for every
safe divergence, real-runtime evidence from the full gate, generated API and
API 0.1/0.2 regression checks, restart/trust/source-change/sanitized-environment
coverage, and a deliberate decision on the 33 TUI audit rows and provider/OAuth
boundary. The deferred shortcut, CLI/flag, session-control, and Pi bridge
remainder lanes require their own bounded contracts and tests before their
statuses can change. Until then this profile remains honestly named dogfood
conformance.
