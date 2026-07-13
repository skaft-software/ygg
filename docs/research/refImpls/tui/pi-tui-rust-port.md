# Reference Implementation: Pi TUI (Rust Port)

> **Reference for Ygg's existing Rust TUI.**
> Pi's Ink/React TUI is not directly portable but documents the expected interactive surface.

---

## Reference Role

**Behavioral reference only.** Ygg already has a Rust TUI. This document captures the interactive behaviors Pi's TUI enables, so Ygg's TUI can support equivalent workflows.

The Rust architecture is not derived from Pi — it is an independent implementation.

---

## Interactive Surface Inventory

### Components

| Component | Purpose | Pi Implementation |
|-----------|---------|-------------------|
| **Message list** | Scrollable conversation history | Ink `Box` with virtual scrolling |
| **Input area** | Multi-line prompt input with history | Ink `TextInput` with custom keybindings |
| **Status bar** | Current model, thinking level, session, token count | Ink `Box` at bottom |
| **Tool output panels** | Collapsible tool execution results | Per-tool `Box` with expand/collapse |
| **Diff viewer** | Side-by-side or unified diff display | Custom Ink component with syntax highlighting |
| **Model picker** | Interactive model selection with search | Ink overlay with fuzzy search |
| **Session picker** | Session list with preview | Ink overlay with session metadata |
| **Approval dialog** | Command/tool approval prompt | Ink modal with yes/no/always options |
| **Thinking display** | Expandable reasoning content | Collapsible `Box` with thinking indicator |

### Key Interactions

| Interaction | Binding | Behavior |
|------------|---------|----------|
| Submit prompt | `Enter` | Send message to agent |
| Newline in prompt | `Shift+Enter` or `Alt+Enter` | Insert newline without submitting |
| Abort active run | `Ctrl+C` | Send abort signal; agent stops at next boundary |
| Cycle model | `Ctrl+P` | Rotate through available models |
| Show model picker | `/model` | Interactive model search and selection |
| Toggle thinking | `/thinking <level>` | Change thinking level for next turn |
| Expand tool output | `Enter` on tool result | Toggle between collapsed/expanded view |
| Compact context | `/compact` | Trigger manual context compaction |
| Reload resources | `Ctrl+R` or `/reload` | Re-scan extensions, skills, prompts, themes |
| Switch theme | `/theme <name>` | Change color scheme at runtime |
| Checkout branch | `/checkout <entryId>` | Fork session from earlier point |
| Show session tree | `/tree` | Display branch structure |
| Resume session | `/resume` | Open session picker |

### Visual States

| State | Visual Indicator |
|-------|-----------------|
| **Agent thinking** | Spinner or animated indicator in status bar |
| **Tool executing** | Tool name + elapsed time in status bar |
| **Streaming text** | Text appears character by character (not chunked) |
| **Streaming thinking** | Dimmed text in collapsible area, marked "Thinking..." |
| **Compaction in progress** | "Compacting context..." in status bar |
| **Error state** | Red-tinted error message with retry option |
| **Aborted** | "Aborted" marker on last message; prompt input re-enabled |
| **Session dirty** | Unsaved indicator (if applicable) |

---

## Theme System

Pi uses TOML-based themes:

```toml
# Example theme structure
name = "my-theme"
variant = "dark"  # dark | light | auto

[colors]
primary = "#6C8EBF"
secondary = "#9673A6"
background = "#1E1E2E"
surface = "#313244"
text = "#CDD6F4"
subtext = "#A6ADC8"
error = "#F38BA8"
warning = "#F9E2AF"
success = "#A6E3A1"
info = "#89B4FA"

[components.message]
user = "#89B4FA"
assistant = "#CDD6F4"
tool = "#A6E3A1"
thinking = "#6C7086"

[components.diff]
addition = "#A6E3A1"
deletion = "#F38BA8"
header = "#89B4FA"

[components.status]
model = "#CBA6F7"
tokens = "#A6ADC8"
```

Themes are loaded at startup, switchable at runtime, and support light/dark/auto variants.

---

## What Ygg Should Support

Ygg's Rust TUI should support these behaviors **eventually**:

1. **Streaming text display** — text appears incrementally as the model generates
2. **Thinking/reasoning visibility** — expandable reasoning content
3. **Tool execution feedback** — live status during tool execution
4. **Model switching** — interactive model selection
5. **Session navigation** — resume, branch, checkout
6. **Compaction feedback** — progress indication during context compression
7. **Error display** — clear error messages with retry options
8. **Theme support** — configurable color schemes
9. **Abort handling** — `Ctrl+C` stops agent cleanly
10. **Steer queuing** — type while agent runs to queue next input

**Not required for v0.1:**

- Full Ink/React component parity
- Extension-contributed UI components
- Interactive approval dialogs (can use simpler confirmation flow)
- Diff viewer with syntax highlighting
- Session tree visualization

---

## Cross-References

| Concern | Reference |
|---------|-----------|
| Product behavior reference | [Pi Coding Agent Reference](../coding-agent/pi-coding-agent.md) |
| Agent execution that feeds the TUI | [Terminus-2 Implementation](../agent/terminus-2.md) |
| Tool execution patterns | [Codex Tools Reference](../coding-agent/codex-tools.md) |
