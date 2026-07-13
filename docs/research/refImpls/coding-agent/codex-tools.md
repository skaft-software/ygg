# Reference Implementation: Codex Tools Subsystem

> **Narrow subsystem reference for shell execution, file patching, permissions, and autonomous CLI behavior.**
> Pi remains the primary coding-product reference; this is a tool-level implementation reference.

---

## Reference Role

**Subsystem reference for `ygg-coding-agent` tool surface design.**

OpenAI's Codex CLI (and to a lesser extent Claude Code) demonstrate patterns for:

- Shell command execution with lifecycle management
- File patching via structured edits
- Permission models for autonomous operation
- Background process management
- Output rendering and truncation
- Interactive approval flows

This document extracts the tool-level implementation patterns without coupling to any specific product architecture.

---

## Shell Execution

### Process Lifecycle

Codex-style shell execution follows a structured lifecycle:

```
                   ┌──────────────────┐
                   │   Command Queued  │
                   └────────┬─────────┘
                            │
                   ┌────────▼─────────┐
                   │  Approval Check   │
                   │ (permission model)│
                   └────────┬─────────┘
                            │
              ┌─────────────┼─────────────┐
              │             │             │
         Auto-approved  Requires      Denied
              │         Approval         │
              │             │             │
              │      ┌──────▼──────┐      │
              │      │ User Prompt │      │
              │      │ (yes/no/    │      │
              │      │  always)    │      │
              │      └──────┬──────┘      │
              │             │             │
              │        ┌────┴────┐        │
              │     Approved   Denied     │
              │        │        │         │
              └────────┴────────┘         │
                       │                  │
              ┌────────▼─────────┐        │
              │   Spawn Process   │       │
              │ (pty or pipe)     │       │
              └────────┬─────────┘       │
                       │                  │
              ┌────────▼─────────┐        │
              │  Stream Output    │       │
              │ (stdout + stderr) │       │
              └────────┬─────────┘       │
                       │                  │
              ┌────────▼─────────┐        │
              │  Wait / Timeout   │       │
              └────────┬─────────┘       │
                       │                  │
            ┌──────────┼──────────┐       │
            │          │          │       │
         Success    Timeout    Error     │
            │          │          │      │
            └──────────┴──────────┘      │
                       │                  │
              ┌────────▼─────────┐        │
              │  Render Result    │ ◄─────┘
              │ (stdout, stderr, │
              │  exit code,       │
              │  duration)        │
              └──────────────────┘
```

### Output Management

Codex tools handle terminal output in two layers:

**Layer 1: Streaming output (live display)**

Output streams to the user as it's generated. This is critical for long-running commands where the user needs to see progress.

**Layer 2: Truncated result (context injection)**

For the model's context, output is truncated:

```
Strategy: Keep first N lines + last N lines
If output > threshold:
    return first_N_lines + "\n... [X lines truncated] ...\n" + last_N_lines

Default: first 2000 lines + last 2000 lines
Configurable per-command type
```

This is the same strategy as Terminus-2's `_limit_output_length()` (first + last halves), applied per-command rather than per-turn.

### Interactive Commands

Codex can run interactive commands (vim, less, TUI applications) by allocating a pseudo-terminal (pty). Key considerations:

- **PTY vs pipe:** PTY for interactive, pipe for scripted. Codex auto-selects based on command characteristics.
- **Keystroke forwarding:** When running interactively, user keystrokes are forwarded to the subprocess.
- **Escape mechanism:** `Ctrl+C` sends SIGINT to the foreground process; double `Ctrl+C` kills the entire tree.
- **Backgrounding:** Long-running servers can be backgrounded. Codex tracks background process PIDs.

### Background Process Management

Codex tracks background processes with:

```
Background process registry:
  - pid
  - command
  - start time
  - output file descriptor (for redirect)
  - status: running | exited | killed

Management:
  - List: show all background processes
  - Kill: send signal to specific PID
  - Output: read output file
  - Wait: block until process exits
```

---

## File Patching

### Structured Edit Operations

Codex uses a structured `edit` tool that performs **string-replace** operations:

```typescript
interface EditOperation {
  filePath: string;          // Absolute or relative to cwd
  oldString: string;         // Exact text to replace (must be unique in file)
  newString: string;         // Replacement text
  expectedReplacements?: number; // Expected count (default: 1)
}
```

**Validation before application:**

1. File exists (or path is valid for new files)
2. `oldString` appears exactly `expectedReplacements` times
3. File size is within limits
4. File is not binary (or explicit binary flag set)

**Application:**

1. Read file contents
2. Replace `oldString` with `newString` (exact match, no regex)
3. Write file (atomic via temp file + rename where possible)
4. Return diff

**Why string-replace instead of line-based:**

- Works with any file format
- No need to understand language syntax
- Exact-match prevents subtle corruption
- Model can specify minimal unique context

### Diff Generation

After each edit, Codex generates a unified diff:

```
@@ -10,7 +10,7 @@
 old line content
-another old line
+replacement line
 more old content
```

The diff is:
1. Shown to the user (in interactive mode)
2. Included in the model's context (so it sees the result)
3. Stored in the session for rollback

### File Mutation Queue

Codex queues file mutations and applies them atomically:

```
Queue:
  - Operation 1: edit file.txt (replace X → Y)
  - Operation 2: write newfile.txt (create with content Z)
  - Operation 3: edit file.txt (replace A → B)

Apply:
  1. Validate all operations
  2. Apply in order
  3. On failure: rollback all (or stop with partial state + error)
```

The queue prevents inconsistent file states when the model produces multiple edit operations in one turn.

---

## Permission Model

### Tiered Permissions

Codex uses a tiered permission model:

| Level | Behavior | Use Case |
|-------|----------|----------|
| **Default-deny** | Every action requires explicit approval | Initial setup, sensitive projects |
| **Default-allow (read)** | Reads auto-approved; writes require approval | Normal use |
| **Default-allow (workspace)** | Reads + writes in workspace auto-approved; external writes require approval | Development workflow |
| **Full-auto** | All actions auto-approved | CI/CD, fully trusted environments |
| **Yolo** | No confirmations, no safety checks | Emergency/debug mode |

### Permission Scope

Permissions are scoped by:

- **Operation type:** read, write, execute, network, file-delete
- **Path:** specific files, directories, or patterns
- **Command:** specific executables or command patterns
- **Duration:** session, 1 hour, permanent

### Permission Persistence

Approved permissions can be persisted:

```
~/.codex/permissions.json:
{
  "rules": [
    {
      "pattern": "npm test",
      "action": "allow",
      "scope": "workspace",
      "expires": null
    },
    {
      "pattern": "rm -rf",
      "action": "deny",
      "scope": "global",
      "expires": null
    }
  ]
}
```

### Interactive Approval UI

When a command requires approval:

```
┌─ Approve command? ──────────────────────────────────────────────┐
│                                                                  │
│  $ npm install --save-dev @types/node                           │
│                                                                  │
│  This command will:                                              │
│  • Install new npm packages                                     │
│  • Modify package.json                                          │
│  • Modify node_modules/                                         │
│                                                                  │
│  [Y] Yes  [N] No  [A] Always  [D] Details  [E] Edit command    │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

---

## Tools Reference

### read

```
Purpose: Read file contents
Parameters:
  - filePath: string (required)
  - offset?: number (line number to start)
  - limit?: number (max lines)
  - encoding?: string (default: utf-8)

Behavior:
  - Validates file exists and is readable
  - Checks file size before reading (reject files > threshold)
  - Returns content with line numbers
  - Returns [toolu_vrtx_xxx] truncated message if file exceeds limit

Edge cases:
  - Binary files: return hex dump or "[Binary file]" marker
  - Empty files: return "[File is empty]"
  - Non-existent: return error
  - Symlinks: follow by default, flag to disable
```

### write

```
Purpose: Create or overwrite a file
Parameters:
  - filePath: string (required)
  - content: string (required)

Behavior:
  - Creates intermediate directories
  - Writes atomically (temp file + rename)
  - Returns confirmation with file size and line count

Edge cases:
  - Existing file: warn in output, proceed
  - Outside workspace: requires elevated permission
  - Directory exists at path: error
```

### edit

```
Purpose: String-replace edit within a file
Parameters:
  - filePath: string (required)
  - oldString: string (required, must be unique in file)
  - newString: string (required)
  - expectedReplacements?: number (default: 1)

Behavior:
  - Validates oldString is unique (or matches expectedReplacements)
  - Performs exact string replacement (not regex)
  - Returns unified diff of changes
  - Atomic via temp file + rename

Edge cases:
  - oldString not found: error with nearby context
  - oldString not unique (when expectedReplacements=1): error showing all matches
  - File unchanged after edit: warn
  - Whitespace sensitivity: exact match including whitespace
```

### bash

```
Purpose: Execute shell commands
Parameters:
  - command: string (required)
  - timeout?: number (seconds, default: 120)
  - workdir?: string (default: cwd)
  - background?: boolean (default: false)
  - env?: Record<string, string>

Behavior:
  - Allocates PTY for interactive commands
  - Streams stdout + stderr interleaved
  - Captures exit code
  - Returns truncated output + exit code + duration

Edge cases:
  - Timeout: SIGTERM → wait 5s → SIGKILL
  - Non-zero exit: return with isError flag
  - Background: return immediately with PID, track in registry
  - Ctrl+C during execution: forward to subprocess
```

### grep

```
Purpose: Search file contents with regex
Parameters:
  - pattern: string (required)
  - path?: string (default: cwd)
  - include?: string (glob pattern for files)
  - exclude?: string (glob pattern for files)
  - maxResults?: number (default: 100)

Behavior:
  - Recursively searches files
  - Returns matching lines with file path and line number
  - Skips binary files
  - Skips .git, node_modules by default

Edge cases:
  - No results: "[No matches found]"
  - Too many results: truncate with count
  - Invalid regex: error with explanation
```

### glob

```
Purpose: Find files matching glob patterns
Parameters:
  - pattern: string (required)
  - path?: string (default: cwd)

Behavior:
  - Returns sorted list of matching paths
  - Relative paths from cwd

Edge cases:
  - No matches: "[No files found]"
  - Too many matches: truncate with count
  - Invalid pattern: error
```

---

## Output Rendering

### Tool Output Format

Codex tools return structured output:

```typescript
interface ToolResult {
  content: string;                    // Human-readable output
  metadata?: {
    exitCode?: number;                // For bash
    duration?: number;                // Execution time in ms
    truncated?: boolean;              // Whether output was truncated
    truncatedCount?: number;          // How many lines/bytes omitted
    diff?: string;                    // For edit: unified diff
    lineCount?: number;               // For read/write: total lines
    matches?: number;                 // For grep: match count
    files?: number;                    // For glob: file count
  };
  isError?: boolean;
}
```

### Truncation Strategy

```
1. Token-aware first: estimate tokens in output
2. If under threshold: return complete output
3. If over threshold:
   a. Keep first ~40% of output (beginning shows command result start)
   b. Keep last ~40% of output (end shows current state / prompt)
   c. Insert truncation marker with byte count
   d. Return: first + marker + last
4. Threshold: ~4000 tokens for context; ~8000 tokens for user display
```

**Truncation marker format:**

```
[... output truncated: 15,432 bytes (1,234 lines) omitted ...]
```

---

## Autonomous CLI Behavior Patterns

### Completion Detection

Autonomous operation requires detecting when a command is "done":

| Signal | Detection |
|--------|-----------|
| Exit code 0 | Command succeeded |
| Exit code non-zero | Command failed (but still "done") |
| Shell prompt appears | Interactive command returned to prompt |
| Timeout | Command likely hung; SIGTERM + SIGKILL |
| Output stalls | No new output for N seconds; may be waiting for input |
| Specific marker | User-defined completion string in output |

### Auto-Approval Patterns

In autonomous mode, Codex uses heuristics for auto-approval:

```
Safe commands (auto-approved by default):
  - Read-only: cat, ls, grep, find, head, tail, wc, stat, file, which, type
  - Version checks: --version, -v
  - Package queries: npm list, pip list, cargo tree

Requires confirmation:
  - Write operations: write, edit
  - Network: curl, wget, npm install, git push
  - Destructive: rm, mv (to outside workspace), chmod, chown
  - System: sudo, shutdown, reboot

Always requires confirmation:
  - rm -rf (any variant)
  - git push --force
  - curl | bash
  - eval, exec, source on unknown scripts
```

### Error Recovery Patterns

When a command fails, Codex uses these recovery patterns:

1. **Retry with fix:** Parse error output, adjust command, retry
2. **Alternative approach:** If command doesn't exist, try alternative (e.g., `python` → `python3`)
3. **Permission escalation:** If "permission denied", suggest `sudo` or `chmod`
4. **Missing dependency:** If tool not found, offer to install
5. **Escalation to user:** If all recovery attempts fail, explain the situation

### Background Task Patterns

For long-running operations:

```
Pattern 1: Fire-and-forget server
  1. Start server in background (npm run dev &)
  2. Record PID
  3. Wait 3 seconds for startup output
  4. Continue with other tasks
  5. Check server health before interacting with it

Pattern 2: Watch mode
  1. Start process (npm test -- --watch)
  2. Model knows it's in watch mode
  3. Sends keystrokes to interact (filter patterns, re-run)
  4. Kill when done

Pattern 3: Parallel operations
  1. Start multiple independent commands
  2. Wait for all to complete
  3. Collect results
```

---

## Summary of Design Patterns

| Pattern | Description | Pi Implementation | Codex Implementation |
|---------|-------------|-------------------|---------------------|
| **Output truncation** | Keep first + last, drop middle | `_limit_output_length()` 10KB | Token-aware, 4000-token threshold |
| **String-replace editing** | Exact string match → replace | `edit` tool with `oldString`/`newString` | Same pattern |
| **Permission tiers** | Graduated autonomy levels | Model-dependent | 5 levels (deny → yolo) |
| **Process lifecycle** | Spawn → stream → wait → result | `bash-executor.ts` with PTY | PTY with background tracking |
| **Atomic writes** | Temp file + rename | `write.ts` creates dirs | Same pattern |
| **Background processes** | Track PID, provide kill/list | `BashExecutor` with operations | Background registry |
| **Approval UI** | Interactive yes/no/always | TUI prompt | Ink-based approval dialog |
| **Completion detection** | Exit code + prompt detection | `send_keys` with timeout | Exit code + output stall detection |

---

## Cross-References

| Concern | Related Reference |
|---------|------------------|
| Pi's bash tool implementation | `core/tools/bash.ts` in Pi coding-agent |
| Pi's edit tool implementation | `core/tools/edit.ts` in Pi coding-agent |
| Terminus-2 output limiting | [Terminus-2 Implementation](../agent/terminus-2.md) § Terminal Output Limiting |
| Terminus-2 command execution | [Terminus-2 Implementation](../agent/terminus-2.md) § Command Execution |
| Pi coding-agent product context | [Pi Coding Agent Reference](./pi-coding-agent.md) |
