# Terminus-2 — Implementation Reference

> **Harbor's reference agent implementation.** A research-preview agent for evaluating language model capabilities in terminal environments. Operates entirely autonomously within sandboxed environments.

---

## Table of Contents

1. [Overview](#overview)
2. [High-Level Architecture](#high-level-architecture)
3. [Class & Data Structures](#class--data-structures)
4. [Core Components](#core-components)
   - [LLM Backend (`LiteLLM`)](#llm-backend-litellm)
   - [Chat Wrapper (`Chat`)](#chat-wrapper-chat)
   - [Tmux Session (`TmuxSession`)](#tmux-session-tmuxsession)
   - [Parsers](#parsers)
   - [Prompt Templates](#prompt-templates)
5. [Agent Lifecycle](#agent-lifecycle)
   - [`perform_task`](#perform_task)
   - [`_run_agent_loop`](#_run_agent_loop)
6. [LLM Interaction Pipeline](#llm-interaction-pipeline)
   - [`_handle_llm_interaction`](#_handle_llm_interaction)
   - [`_query_llm`](#_query_llm)
7. [Command Execution](#command-execution)
8. [Task Completion (Double Confirmation)](#task-completion-double-confirmation)
9. [Conversation History Management](#conversation-history-management)
   - [Proactive Summarization](#proactive-summarization)
   - [Passive Summarization (Error Recovery)](#passive-summarization-error-recovery)
   - [The 3-Step Standard Summarization](#the-3-step-standard-summarization)
10. [Error Handling](#error-handling)
    - [ContextLengthExceededError](#contextlengthexceedederror)
    - [OutputLengthExceededError](#outputlengthexceedederror)
11. [Terminal Output Limiting](#terminal-output-limiting)
12. [Configuration Options](#configuration-options)
13. [Reinforcement Learning Support](#reinforcement-learning-support)
14. [Trajectory Format (ATIF)](#trajectory-format-atif)
15. [Full Source Reference](#full-source-reference)

---

## Overview

Terminus-2 is a single-tool autonomous agent that interacts with terminal environments through a tmux session. Its design philosophy centers on three principles:

| Principle | Description |
|-----------|-------------|
| **Mono-tool** | Only one "tool" — an interactive tmux session. The agent sends raw keystrokes, not structured tool calls. |
| **Independence** | Agent logic runs in a separate Python process from the Docker container. Clean separation of concerns. |
| **Autonomy-first** | Never asks the user for input. Makes all decisions and recovers from errors independently. |

### Mono-tool Design Rationale

Instead of defining hundreds of tools like `run_command`, `read_file`, `list_directory`, Terminus-2 uses a single tmux session. The model sends **keystroke sequences** (`Ctrl-c`, `ls -la\n`, arrow keys, etc.) and the agent types them into tmux. This means the agent can:

- Send keystrokes and navigate environments flexibly
- Scroll through output and use arrow keys to navigate menus
- Launch additional shells within the environment
- Interact with **any** terminal-based application naturally (vim, less, ssh, python repl, etc.)

No specialized tool is needed for each interaction pattern — the model uses the same interface a human would.

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                        Harbor Orchestrator                           │
│                                                                      │
│  ┌────────────────────┐          ┌──────────────────────────────┐   │
│  │   Terminus2 Agent  │          │     Docker Container          │   │
│  │   (Python process)  │  tmux    │                              │   │
│  │                    │◄────────►│  ┌────────────────────────┐  │   │
│  │  ┌──────────────┐  │ session  │  │   Tmux Session          │  │   │
│  │  │   LiteLLM    │  │          │  │   (bash shell)          │  │   │
│  │  │   (model)    │  │          │  │                        │  │   │
│  │  └──────┬───────┘  │          │  │   $ ls                  │  │   │
│  │         │          │          │  │   $ cat file.txt        │  │   │
│  │  ┌──────▼───────┐  │          │  │   $ python script.py    │  │   │
│  │  │    Chat      │  │          │  │                        │  │   │
│  │  │  (history)   │  │          │  └────────────────────────┘  │   │
│  │  └──────┬───────┘  │          │                              │   │
│  │         │          │          └──────────────────────────────┘   │
│  │  ┌──────▼───────┐  │                                             │
│  │  │   Parser     │  │                                             │
│  │  │ (json/xml)   │  │                                             │
│  │  └──────┬───────┘  │                                             │
│  │         │          │                                             │
│  │  ┌──────▼───────┐  │                                             │
│  │  │ Agent Loop   │  │                                             │
│  │  │ (episodes)   │  │                                             │
│  │  └──────────────┘  │                                             │
│  └────────────────────┘                                             │
└──────────────────────────────────────────────────────────────────────┘
```

**Key separation:** The agent's Python process (LLM calls, parsing, loop logic) runs **outside** the container. Only keystrokes are sent into the container via tmux. This enables remote connection to arbitrary environments and clean isolation.

---

## Class & Data Structures

### `Command` (dataclass)

```python
@dataclass
class Command:
    keystrokes: str        # Raw keystrokes to send to tmux (e.g., "ls -la\n")
    duration_sec: float    # Minimum time to wait after sending, for output to appear
```

A `Command` is the atomic unit of action. The parser produces a list of these from the model's structured output. Duration is capped at 60 seconds.

### `Terminus2` (extends `BaseAgent`)

```python
class Terminus2(BaseAgent):
    def __init__(
        self,
        model_name: str,                     # e.g., "openai/gpt-5"
        max_episodes: int | None = None,     # Max turns (default: effectively unlimited = 1,000,000)
        parser_name: str = "json",           # "json" or "xml"
        api_base: str | None = None,         # Custom API endpoint for vLLM etc.
        temperature: float = 0.7,            # Sampling temperature
        **kwargs,
    ):
```

#### Instance Attributes

| Attribute | Type | Description |
|-----------|------|-------------|
| `_model_name` | `str` | Model identifier passed to LiteLLM |
| `_parser_name` | `str` | `"json"` or `"xml"` |
| `_llm` | `LiteLLM` | The LLM backend client |
| `_parser` | `TerminusJSONPlainParser` or `TerminusXMLPlainParser` | Response parser |
| `_prompt_template` | `str` | System prompt template (read from file) |
| `_timeout_template` | `str` | Timeout error message template |
| `_logger` | `Logger` | Child logger |
| `_max_episodes` | `int` | Max turns (default 1,000,000) |
| `_chat` | `Chat \| None` | Conversation history (initialized in `perform_task`) |
| `_timestamped_markers` | `list[tuple[float, str]]` | Debug markers for asciinema replay |
| `_pending_completion` | `bool` | Tracks first "task complete" for double-confirmation |

#### Key Methods

| Method | Purpose |
|--------|---------|
| `perform_task()` | Entry point. Creates Chat, builds initial prompt, runs agent loop. |
| `_run_agent_loop()` | Main episode loop. Fetches model response, executes commands, manages summarization. |
| `_handle_llm_interaction()` | Sends prompt to LLM, parses response, returns commands + completion flag + feedback. |
| `_query_llm()` | Sends prompt via Chat, handles `ContextLengthExceededError` and `OutputLengthExceededError` with retry. |
| `_execute_commands()` | Types keystrokes into tmux session, captures output. |
| `_summarize()` | 3-step subagent summarization process. |
| `_check_proactive_summarization()` | Checks if token usage exceeds threshold; triggers summarization. |
| `_unwind_messages_to_free_tokens()` | Drops recent message pairs to free context space. |
| `_limit_output_length()` | Truncates terminal output to max bytes (default 10,000). |
| `_count_total_tokens()` | Counts tokens across all chat messages. |
| `_get_model_context_limit()` | Returns the model's context window size. |

---

## Core Components

### LLM Backend (`LiteLLM`)

The `LiteLLM` class wraps LiteLLM's completion API. It provides:

- `call(prompt, message_history)` — Makes a raw LLM call with optional message history
- Temperature and API base configuration at construction time
- Custom model info registration for token counting

The LLM is instantiated once and reused across episodes:

```python
self._llm = LiteLLM(
    model_name=model_name,
    api_base=api_base,
    temperature=temperature
)
```

### Chat Wrapper (`Chat`)

`Chat` manages the conversation message list and provides a `chat()` method that:

1. Appends the user's prompt to `_messages`
2. Calls `self._model.call(prompt=prompt, message_history=self._messages)`
3. Appends the assistant's response to `_messages`
4. Returns the response text

It also tracks cumulative token usage:
- `total_input_tokens`
- `total_output_tokens`

These are recorded in `AgentResult` at task completion.

### Tmux Session (`TmuxSession`)

The tmux session is the agent's only interface to the environment. Key methods used by Terminus-2:

| Method | Purpose |
|--------|---------|
| `send_keys(keystrokes, block, min_timeout_sec)` | Types keystrokes into the tmux pane. If `block=True`, waits for output to settle (raises `TimeoutError` if no output within timeout). |
| `get_incremental_output()` | Returns new output since the last call (diff-based). |
| `capture_pane(capture_entire)` | Captures the full visible tmux pane. Used during summarization for the "current screen" context. |
| `is_session_alive()` | Checks if the tmux session is still running. |
| `get_asciinema_timestamp()` | Returns current asciinema timestamp for debug markers. |

### Parsers

Terminus-2 supports two structured output formats, each with its own parser.

#### `TerminusJSONPlainParser`

Parses JSON responses. Expected format (from the prompt template):

```json
{
    "analysis": "...",
    "plan": "...",
    "commands": [
        {"keystrokes": "ls -la\n", "duration": 3.0},
        {"keystrokes": "cat file.txt\n", "duration": 1.0}
    ],
    "task_complete": false
}
```

The parser returns a `ParseResult` with:

| Field | Type | Description |
|-------|------|-------------|
| `commands` | `list[ParsedCommand]` | Parsed command objects with keystrokes and duration |
| `is_task_complete` | `bool` | Whether the model declared the task complete |
| `error` | `str \| None` | Fatal parsing error (unrecoverable) |
| `warning` | `str \| None` | Non-fatal warning (parsing succeeded but something is off) |

#### `TerminusXMLPlainParser`

Parses XML-tagged responses. Expected format (from the prompt template):

```xml
<analysis>...</analysis>
<plan>...</plan>
<commands>
    <command>
        <keystrokes>ls -la\n</keystrokes>
        <duration>3.0</duration>
    </command>
</commands>
<task_complete>false</task_complete>
```

Additionally provides a `salvage_truncated_response()` method that attempts to recover a valid response from truncated XML (used in `OutputLengthExceededError` recovery).

#### ParseResult Flow

```
Model Response
      │
      ▼
┌─────────────┐
│   Parser    │
│ .parse()    │
└──────┬──────┘
       │
       ▼
┌─────────────────────────────────────┐
│  ParseResult                        │
│  ├── commands: list[ParsedCommand]  │
│  ├── is_task_complete: bool         │
│  ├── error: str | None              │
│  └── warning: str | None            │
└─────────────────────────────────────┘
       │
       ├── If error: Feedback loop (re-prompt with error)
       ├── If warning: Include in next prompt
       └── If OK: Execute commands
```

### Prompt Templates

Two prompt template files are loaded at initialization:

1. **Main prompt** — `prompt-templates/terminus-json-plain.txt` or `terminus-xml-plain.txt`
   - Contains the system prompt with format instructions
   - Has `{instruction}` and `{terminal_state}` placeholders
   - Formatted at the start of `perform_task()`

2. **Timeout template** — `prompt-templates/timeout.txt`
   - Contains `{timeout_sec}`, `{command}`, `{terminal_state}` placeholders
   - Used when a command times out (no output within `duration_sec`)

The correct template is selected based on `parser_name`:

```python
def _get_prompt_template_path(self) -> Path:
    if self._parser_name == "json":
        return Path(__file__).parent.parent / "prompt-templates/terminus-json-plain.txt"
    elif self._parser_name == "xml":
        return Path(__file__).parent.parent / "prompt-templates/terminus-xml-plain.txt"
```

---

## Agent Lifecycle

### `perform_task`

```python
def perform_task(
    self,
    instruction: str,
    session: TmuxSession,
    logging_dir: Path | None = None,
    time_limit_seconds: float | None = None,
) -> AgentResult:
```

This is the entry point called by Harbor.

**Steps:**

1. **Create Chat**: `chat = Chat(self._llm)` — fresh conversation history.
2. **Build initial prompt**: Format the prompt template with `{instruction}` and initial `{terminal_state}`.
3. **Run agent loop**: `self._run_agent_loop(initial_prompt, session, chat, logging_dir, instruction)`.
4. **Return result**: `AgentResult` with token counts, failure mode, and debug markers.

```python
initial_prompt = self._prompt_template.format(
    instruction=instruction,
    terminal_state=self._limit_output_length(session.get_incremental_output()),
)

self._run_agent_loop(initial_prompt, session, chat, logging_dir, instruction)

return AgentResult(
    total_input_tokens=chat.total_input_tokens,
    total_output_tokens=chat.total_output_tokens,
    failure_mode=FailureMode.NONE,
    timestamped_markers=self._timestamped_markers,
)
```

### `_run_agent_loop`

The core execution loop. Runs up to `_max_episodes` turns.

```python
def _run_agent_loop(
    self,
    initial_prompt: str,
    session: TmuxSession,
    chat: Chat,
    logging_dir: Path | None = None,
    original_instruction: str = "",
) -> None:
```

**Per-episode flow:**

```
┌──────────────────────────────────────────────────────────┐
│                    Episode Loop                           │
│                                                          │
│  1. Check session alive ──────────► Exit if dead         │
│                                                          │
│  2. Proactive summarization check                        │
│     (if free_tokens < 8000, summarize conversation)      │
│                                                          │
│  3. Setup episode logging                                │
│     (debug.json, prompt.txt, response.txt per episode)   │
│                                                          │
│  4. _handle_llm_interaction(prompt)                      │
│     ├── Send prompt to LLM                               │
│     ├── Parse response                                   │
│     └── Return (commands, is_task_complete, feedback)    │
│                                                          │
│  5. Record asciinema marker                              │
│                                                          │
│  6. If parse error → Set prompt to error, continue       │
│                                                          │
│  7. _execute_commands(commands, session)                 │
│     └── Return (timeout_occurred, terminal_output)       │
│                                                          │
│  8. Double-confirmation check for task_complete          │
│                                                          │
│  9. Build next prompt with terminal output (+ warnings)  │
│                                                          │
│  10. Loop to next episode                                │
└──────────────────────────────────────────────────────────┘
```

**Detailed per-step behavior:**

#### Step 1: Session check

```python
if not session.is_session_alive():
    self._logger.info("Session has ended, breaking out of agent loop")
    break
```

#### Step 2: Proactive summarization

```python
if original_instruction:
    proactive_summary = self._check_proactive_summarization(
        chat, original_instruction, session
    )
    if proactive_summary:
        prompt = proactive_summary
```

See [Proactive Summarization](#proactive-summarization) for details.

#### Step 3: Episode logging

```python
logging_paths = self._setup_episode_logging(logging_dir, episode)
```

If `logging_dir` is provided, creates `episode-{N}/` subdirectory with:
- `debug.json` — Full LLM interaction debug log
- `prompt.txt` — The exact prompt sent
- `response.txt` — The raw LLM response

#### Step 4: LLM interaction

```python
commands, is_task_complete, feedback = self._handle_llm_interaction(
    chat, prompt, logging_paths, original_instruction, session
)
```

See [LLM Interaction Pipeline](#llm-interaction-pipeline) for full details.

#### Step 5: Debug marker

```python
self._record_asciinema_marker(
    f"Episode {episode}: {len(commands)} commands", session
)
```

Records a timestamped marker for asciinema replay debugging.

#### Step 6: Parse error handling

```python
if feedback and "ERROR:" in feedback:
    prompt = (
        f"Previous response had parsing errors:\n{feedback}\n\n"
        f"Please fix these issues and provide a proper "
        f"{self._get_error_response_type()}."
    )
    continue
```

If the parser returned an error (invalid JSON/XML, missing required fields), the prompt for the next episode is the error feedback. No commands are executed. The loop continues to the next episode with this error prompt.

`_get_error_response_type()` returns `"JSON response"` for json parser, `"response"` for xml.

#### Step 7: Command execution

```python
timeout_occurred, terminal_output = self._execute_commands(commands, session)
```

See [Command Execution](#command-execution) for details.

#### Step 8: Task completion (double confirmation)

```python
if is_task_complete:
    if self._pending_completion:
        break            # Second confirm → exit
    else:
        self._pending_completion = True
        prompt = self._get_completion_confirmation_message(terminal_output)
        continue         # First confirm → ask again
else:
    self._pending_completion = False
```

See [Task Completion](#task-completion-double-confirmation) for details.

#### Step 9: Build next prompt

```python
if feedback and "WARNINGS:" in feedback:
    prompt = (
        f"Previous response had warnings:\n{feedback}\n\n"
        f"{self._limit_output_length(terminal_output)}"
    )
else:
    prompt = self._limit_output_length(terminal_output)
```

The next prompt is the terminal output (truncated if needed), prefixed with any parser warnings.

---

## LLM Interaction Pipeline

### `_handle_llm_interaction`

```python
def _handle_llm_interaction(
    self,
    chat: Chat,
    prompt: str,
    logging_paths: tuple[Path | None, Path | None, Path | None],
    original_instruction: str = "",
    session: TmuxSession | None = None,
) -> tuple[list[Command], bool, str]:
```

**Flow:**

```
prompt
   │
   ▼
_query_llm(chat, prompt, ...)  ───►  raw response string
   │
   ▼
parser.parse_response(response)  ───►  ParseResult
   │
   ▼
Build feedback string:
  - If error:   "ERROR: {error}"
  - If warning: "WARNINGS: {warning}"
  - Both:       "ERROR: {error}\nWARNINGS: {warning}"
   │
   ▼
Convert ParsedCommands → Commands
  (cap duration at 60s)
   │
   ▼
Return (commands, is_task_complete, feedback)
```

**Command duration capping:**

```python
commands = []
for parsed_cmd in result.commands:
    commands.append(
        Command(
            keystrokes=parsed_cmd.keystrokes,
            duration_sec=min(parsed_cmd.duration, 60),
        )
    )
```

Each command's duration is capped at 60 seconds as a safety measure.

### `_query_llm`

```python
@retry(stop=stop_after_attempt(3))
def _query_llm(
    self,
    chat: Chat,
    prompt: str,
    logging_paths: tuple[Path | None, Path | None, Path | None],
    original_instruction: str = "",
    session: TmuxSession | None = None,
) -> str:
```

Decorated with `@retry(stop=stop_after_attempt(3))` from tenacity — retries up to 3 times on failure.

**Normal flow (no errors):**

1. Write prompt to `prompt.txt` if logging enabled
2. Call `chat.chat(prompt, logging_path=logging_path)`
3. Write response to `response.txt` if logging enabled
4. Return response string

**Error handling branches:**

#### Branch 1: `ContextLengthExceededError`

```python
except ContextLengthExceededError:
    self._logger.info("Context length exceeded. Unwinding messages and summarizing.")

    if session is None:
        raise RuntimeError("Cannot handle context length error without session")

    # Step 1: Unwind
    self._unwind_messages_to_free_tokens(chat, target_free_tokens=4000)

    # Step 2: Summarize
    summary = self._summarize(chat, original_instruction, session)

    # Step 3: Re-attempt with summary
    summary_prompt = f"{summary}\n\n{prompt}"
    if prompt_path is not None:
        prompt_path.write_text(summary_prompt)

    response = chat.chat(summary_prompt, logging_path=logging_path)
    if response_path is not None:
        response_path.write_text(response)
    return response
```

#### Branch 2: `OutputLengthExceededError`

```python
except OutputLengthExceededError as e:
    self._logger.info(f"Output length exceeded: {e}")

    truncated_response = getattr(e, "truncated_response", "[TRUNCATED RESPONSE NOT AVAILABLE]")

    # Attempt salvage (XML parser only)
    salvaged_response = None
    if hasattr(self._parser, "salvage_truncated_response"):
        salvaged_response, has_multiple_blocks = (
            self._parser.salvage_truncated_response(truncated_response)
        )

    if salvaged_response:
        # Valid response found in truncated output — use it!
        self._logger.debug("Output exceeded length but found valid response. Using truncated version.")
        if response_path is not None:
            response_path.write_text(salvaged_response)
        return salvaged_response

    # Cannot salvage — build error message
    # Try to parse truncated response for warnings
    warnings_text = ""
    try:
        parse_result = self._parser.parse_response(truncated_response)
        if parse_result.warning:
            warnings_text = f"\n\nParser warnings from your truncated response:\n{parse_result.warning}"
    except Exception as parse_error:
        self._logger.debug(f"Failed to parse truncated response: {parse_error}")

    error_msg = (
        "ERROR!! NONE of the actions you just requested were performed "
        "because you exceeded the maximum output length of 4096 tokens. "
        "Your outputs must be less than 4096 tokens. Re-issue this request, "
        "breaking it into chunks each of which is less than 4096 tokens."
    )

    if warnings_text:
        error_msg += warnings_text

    # Manually add truncated response + error to chat history
    chat._messages.append({"role": "user", "content": prompt})
    chat._messages.append({"role": "assistant", "content": truncated_response})
    chat._messages.append({"role": "user", "content": error_msg})

    if response_path is not None:
        response_path.write_text(error_msg)

    # Recurse with error message as new prompt
    return self._query_llm(
        chat=chat,
        prompt=error_msg,
        logging_paths=logging_paths,
        original_instruction=original_instruction,
        session=session,
    )
```

Key detail: The truncated response is manually appended to chat history so the model can see what it already sent. Then `_query_llm` recursively calls itself with the error message as the new prompt, so the model gets a chance to produce a shorter response.

#### Branch 3: Generic `Exception`

```python
except Exception as e:
    self._logger.error(f"Unknown Error in LLM interaction: {e}")
    raise e
```

Unknown errors are logged and re-raised. The `@retry` decorator will retry up to 3 times.

---

## Command Execution

### `_execute_commands`

```python
def _execute_commands(
    self,
    commands: list[Command],
    session: TmuxSession,
) -> tuple[bool, str]:
```

**Flow:**

```
For each command in commands:
    │
    ├── session.send_keys(
    │       command.keystrokes,
    │       block=False,                  # Don't wait for output to settle
    │       min_timeout_sec=command.duration_sec  # But enforce minimum wait
    │   )
    │
    ├── If TimeoutError:
    │       return (True, timeout_template.format(
    │           timeout_sec=command.duration_sec,
    │           command=command.keystrokes,
    │           terminal_state=_limit_output_length(
    │               session.get_incremental_output()
    │           ),
    │       ))
    │
    └── Continue to next command

After all commands:
    return (False, _limit_output_length(
        session.get_incremental_output()
    ))
```

**Key behaviors:**

- `block=False`: The agent doesn't wait for the command to finish before sending the next one. This is crucial for sequences like `vim\n` followed by `:wq\n` — the keystrokes are fired rapidly.
- `min_timeout_sec`: A floor on wait time. Even with `block=False`, each `send_keys` call waits at least this long, ensuring commands have time to produce output.
- Timeout handling: If a command produces no output within `duration_sec`, a `TimeoutError` is raised. The agent returns early with a timeout template message that becomes the next prompt, informing the model that the command timed out.
- Output is captured once after **all** commands in the batch execute (via `get_incremental_output()`), not after each individual command.
- Output is truncated via `_limit_output_length()` to 10,000 bytes (default).

---

## Task Completion (Double Confirmation)

Terminus-2 uses a **double-confirmation** pattern for task completion, preventing premature termination:

```
Model says "task_complete: true"
    │
    ▼
┌─────────────────────────────────────────────┐
│  First occurrence:                          │
│    _pending_completion = True               │
│    Send confirmation prompt:                │
│    "Are you sure? Current state: ..."       │
│    Continue loop (do NOT exit)              │
└─────────────────────────────────────────────┘
    │
    ▼
Next episode: Model either:
    │
    ├── Confirms (task_complete: true again)
    │       → Break the loop, task done
    │
    └── Does NOT confirm (task_complete: false)
            → _pending_completion = False
            → Continue working
```

**Implementation:**

```python
if is_task_complete:
    if self._pending_completion:
        # Second consecutive task complete — actually complete
        break
    else:
        # First task complete — ask for confirmation
        self._pending_completion = True
        prompt = self._get_completion_confirmation_message(terminal_output)
        continue
else:
    # Reset pending completion if they didn't confirm
    self._pending_completion = False
```

**Confirmation message format:**

For JSON parser:
```
Current terminal state:
{terminal_output}

Are you sure you want to mark the task as complete? This will trigger your
solution to be graded and you won't be able to make any further corrections.
If so, include "task_complete": true in your JSON response again.
```

For XML parser:
```
Current terminal state:
{terminal_output}

Are you sure you want to mark the task as complete? This will trigger your
solution to be graded and you won't be able to make any further corrections.
If so, include <task_complete>true</task_complete> again.
```

This mechanism ensures the model has a chance to see the final terminal state before committing to task completion.

---

## Conversation History Management

Terminus-2 implements two forms of summarization to handle long-running tasks within context window limits.

### Proactive Summarization

Triggered **before** an LLM call when free tokens drop below the threshold.

```python
def _check_proactive_summarization(
    self, chat: Chat, original_instruction: str, session: TmuxSession
) -> str | None:
    context_limit = self._get_model_context_limit()
    current_tokens = self._count_total_tokens(chat)
    free_tokens = context_limit - current_tokens

    if free_tokens < 8000:  # Hardcoded threshold
        self._logger.debug(f"Proactively summarizing. Free tokens: ~{free_tokens}")
        summary = self._summarize(chat, original_instruction, session)
        return summary

    return None
```

The threshold is **8000 tokens** (hardcoded in `_run_agent_loop`'s call, not configurable in the base class; in Harbor it's configurable via `proactive_summarization_threshold` in agent config kwargs).

When triggered, the return value replaces the next `prompt` variable — the next LLM call gets the handoff/summary prompt instead of the usual terminal output.

### Passive Summarization (Error Recovery)

Triggered when `ContextLengthExceededError` is caught in `_query_llm`. Uses a multi-step fallback strategy:

```
ContextLengthExceededError
         │
         ▼
┌──────────────────────────────────────────────┐
│ Step 1: Unwind messages                      │
│   Remove user+assistant pairs from end       │
│   until free_tokens >= 4000                  │
│   Always keeps at least the first message    │
└──────────────────────┬───────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────┐
│ Step 2: Standard Summarization               │
│   3-step subagent process (see below)        │
└──────────────────────┬───────────────────────┘
                       │
                  ┌────┴────┐
                  │         │
              Success    Failure
                  │         │
                  │         ▼
                  │   ┌──────────────────────────┐
                  │   │ Step 3: Fallback Summary  │
                  │   │   Only: System prompt +   │
                  │   │   Task + Current state    │
                  │   └──────────────────────────┘
                  │         │
                  │    ┌────┴────┐
                  │    │         │
                  │ Success   Failure
                  │    │         │
                  │    │         ▼
                  │    │   ┌──────────────────────┐
                  │    │   │ Step 4: Ultimate      │
                  │    │   │   Fallback            │
                  │    │   │   Continue without    │
                  │    │   │   summarization       │
                  │    │   └──────────────────────┘
                  │    │
                  └────┴──────────
                       │
                       ▼
              Continue execution
```

#### Step 1: Unwind

```python
def _unwind_messages_to_free_tokens(
    self, chat: Chat, target_free_tokens: int = 4000
) -> None:
    context_limit = self._get_model_context_limit()

    while len(chat._messages) > 1:  # Keep at least the first message
        current_tokens = self._count_total_tokens(chat)
        free_tokens = context_limit - current_tokens

        if free_tokens >= target_free_tokens:
            break

        # Remove the most recent pair (user + assistant)
        if len(chat._messages) >= 2:
            chat._messages = chat._messages[:-2]
        else:
            break
```

Removes the most recent user+assistant message pairs from the end until at least `target_free_tokens` (4000) are free, or only the first message remains.

### The 3-Step Standard Summarization

Used by both proactive and passive summarization. Implemented in `_summarize()`.

```python
def _summarize(
    self, chat: Chat, original_instruction: str, session: TmuxSession
) -> str:
```

#### Step 1 — Summary Subagent

The current Chat's LLM is asked to summarize its own conversation:

```python
summary_prompt = f"""You are about to hand off your work to another AI agent.
Please provide a comprehensive summary of what you have accomplished so far on this task:

Original Task: {original_instruction}

Based on the conversation history, please provide a detailed summary covering:
1. **Major Actions Completed** - List each significant command you executed
   and what you learned from it.
2. **Important Information Learned** - A summary of crucial findings, file
   locations, configurations, error messages, or system state discovered.
3. **Challenging Problems Addressed** - Any significant issues you
   encountered and how you resolved them.
4. **Current Status** - Exactly where you are in the task completion process.

Be comprehensive and detailed. The next agent needs to understand everything
that has happened so far in order to continue."""

summary_response = chat.chat(summary_prompt)
```

The summary is generated using the **existing Chat's message history** — the model sees the full conversation to produce the summary.

#### Step 2 — Question Subagent

A **fresh** LLM call (no message history) generates questions based only on the summary:

```python
current_screen = session.capture_pane(capture_entire=False)

question_prompt = f"""You are picking up work from a previous AI agent on this task:

**Original Task:** 
{original_instruction}

**Summary from Previous Agent:**
{summary_response}

**Current Terminal Screen:**
{current_screen}

Please begin by asking several questions (at least five, more if necessary)
about the current state of the solution that are not answered in the
summary from the prior agent. After you ask these questions you will
be on your own, so ask everything you need to know.
"""

model_questions = chat._model.call(prompt=question_prompt, message_history=[])
```

Note: `chat._model.call()` is called **directly** with empty `message_history=[]`, bypassing the Chat wrapper. This means a fresh context for the question generation.

#### Step 3 — Answer Subagent

The original Chat answers the questions:

```python
model_answers = chat.chat(
    "The next agent has a few questions for you, please answer each "
    "of them one by one in detail:\n\n" + model_questions
)
```

#### Step 4 — Compress Conversation

The chat history is replaced with only:

```python
chat._messages = [
    chat._messages[0],                                    # System prompt (first message)
    {"role": "user", "content": question_prompt},         # The question prompt
    {"role": "assistant", "content": model_questions},    # The model's questions
]
```

#### Step 5 — Return Handoff Prompt

```python
handoff_prompt = (
    "Here are the answers the other agent provided.\n\n"
    + model_answers
    + "\n\n"
    + "Continue working on this task from where the previous agent left off."
    " You can no longer ask questions. Please follow the spec to interact with "
    "the terminal."
)

return handoff_prompt
```

The returned handoff prompt is prepended to the next user message. It contains the answers to the questions, plus an instruction to continue working.

**Result of summarization on the conversation:**

```
Before summarization:
  [system_prompt, user_1, assistant_1, user_2, assistant_2, ..., user_N, assistant_N]

After summarization:
  [system_prompt, question_prompt, model_questions]
  
Next prompt sent to model:
  "Here are the answers...\n{model_answers}\n\nContinue working...\n\n{terminal_output}"
```

The middle portion of the conversation is replaced by the compressed summary + Q&A exchange, while the task instruction and system prompt are preserved.

---

## Error Handling

### ContextLengthExceededError

Caught in `_query_llm`. Full recovery path:

1. **Unwind**: Remove recent message pairs until ~4000 tokens are free
2. **Summarize**: Run 3-step summarization on remaining messages
3. **Re-attempt**: Prepend summary handoff to original prompt, call `chat.chat()` again

If summarization fails at any point, falls back to:
- Fallback summary (system prompt + task + current state only)
- Ultimate fallback (continue without any summary, just system prompt + task + state)

### OutputLengthExceededError

Caught in `_query_llm`. Recovery path:

1. **Salvage** (XML parser only): `salvage_truncated_response()` attempts to find valid XML in the truncated output. If found, use it and continue.
2. **Parse for warnings**: Try to parse the truncated response to extract parser warnings.
3. **Build error message**: Inform the model it exceeded 4096 output tokens, include any parser warnings.
4. **Manual history injection**: Append the truncated response and error message directly to `chat._messages`.
5. **Recursive retry**: Call `_query_llm` again with the error message as prompt — the model will see what it already sent and try a shorter version.

The manual history injection is critical: without it, the model wouldn't see its truncated output on the next attempt, because `chat.chat()` normally only appends the response after a successful call. By manually adding the truncated response and error, the model has full context of the failure.

---

## Terminal Output Limiting

```python
def _limit_output_length(self, output: str, max_bytes: int = 10000) -> str:
    if len(output.encode("utf-8")) <= max_bytes:
        return output

    portion_size = max_bytes // 2
    output_bytes = output.encode("utf-8")

    first_portion = output_bytes[:portion_size].decode("utf-8", errors="ignore")
    last_portion = output_bytes[-portion_size:].decode("utf-8", errors="ignore")

    omitted_bytes = (
        len(output_bytes)
        - len(first_portion.encode("utf-8"))
        - len(last_portion.encode("utf-8"))
    )

    return (
        f"{first_portion}\n[... output limited to {max_bytes} bytes; "
        f"{omitted_bytes} interior bytes omitted ...]\n{last_portion}"
    )
```

**Strategy:** Byte-accurate truncation that preserves both the **beginning** and **end** of output. Each gets `max_bytes // 2` (5000 by default). The middle is replaced with an informational message including the byte count. This is crucial because terminal output often has critical information at both the top (command output start) and bottom (prompt line, current state).

The limit is applied to:
- Terminal output from `get_incremental_output()` before it's sent to the LLM
- Terminal state in timeout error messages
- Initial terminal state in `perform_task()`

---

## Configuration Options

### Constructor Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `model_name` | `str` | *(required)* | Model identifier, e.g. `"openai/gpt-5"` |
| `max_episodes` | `int \| None` | `None` (→ 1,000,000) | Maximum number of agent turns |
| `parser_name` | `str` | `"json"` | Output format: `"json"` or `"xml"` |
| `api_base` | `str \| None` | `None` | Custom API endpoint (vLLM, etc.) |
| `temperature` | `float` | `0.7` | Sampling temperature |

### Harbor AgentConfig (extended kwargs)

When used through Harbor, additional options are passed via `AgentConfig.kwargs`:

```python
AgentConfig(
    name=AgentName.TERMINUS_2,
    model_name="openai/gpt-5",
    kwargs={
        "parser_name": "json",              # "json" or "xml"
        "api_base": "https://...",           # Custom API endpoint
        "temperature": 0.7,                  # Sampling temperature
        "max_turns": 100,                    # Max episodes (overrides default 1,000,000)
        "enable_summarize": True,            # Enable context summarization
        "proactive_summarization_threshold": 8000,  # Free tokens threshold
        "collect_rollout_details": False,    # RL: collect token IDs + logprobs
        "reasoning_effort": "medium",        # OpenAI reasoning effort level
        "max_thinking_tokens": 2048,         # Anthropic extended thinking
        "model_info": {                      # Custom model token/pricing info
            "max_input_tokens": 128000,
            "max_output_tokens": 4096,
            "input_cost_per_token": 0.000003,
            "output_cost_per_token": 0.000015,
        },
        "session_id": "custom-session-id",   # Custom session ID for tracking
    }
)
```

---

## Reinforcement Learning Support

Terminus-2 collects detailed rollout information for RL training pipelines.

### Rollout Details Collection

When `collect_rollout_details=True` (in Harbor's agent config kwargs), Terminus-2 collects per-turn:

| Field | Type | Description |
|-------|------|-------------|
| `prompt_token_ids` | `list[list[int]]` | Token ID sequences per turn (full prompt including chat history) |
| `completion_token_ids` | `list[list[int]]` | Token ID sequences per turn (response tokens) |
| `logprobs` | `list[list[float]]` | Log probability sequences per completion token |

These are stored as a list of `RolloutDetail` objects in `AgentResult.metadata["rollout_details"]`:

```python
# First RolloutDetail contains main agent conversation
rollout_detail = trial_result.agent_result.metadata["rollout_details"][0]

prompt_token_ids = rollout_detail["prompt_token_ids"]       # List[List[int]]
completion_token_ids = rollout_detail["completion_token_ids"] # List[List[int]]
logprobs = rollout_detail["logprobs"]                       # List[List[float]]
```

### Rewards

Rewards come from Harbor's verifier system:

```python
reward = trial_result.verifier_result.rewards.get("reward", 0)
```

---

## Trajectory Format (ATIF)

Terminus-2 generates trajectories in the **Agent Trajectory Interchange Format (ATIF)**, Harbor's standardized format.

### TrajectoryConfig

Controls how trajectories are recorded:

```python
@dataclass
class TrajectoryConfig:
    raw_content: bool = False     # True = save raw LLM responses; False = parsed structured data
    linear_history: bool = False  # True = split files on summarization; False = single file
```

#### `raw_content`

| Value | Behavior | Use Case |
|-------|----------|----------|
| `False` (default) | Saves parsed data: separate `message` and `tool_calls` fields | Debugging, analysis |
| `True` | Saves exact raw LLM response in `message` field | SFT dataset generation (needs exact model outputs) |

#### `linear_history`

Controls how summarization boundaries are reflected in trajectory files.

**`linear_history=False` (default):**

All main agent steps in a single `trajectory.json`. Summarization subagents in separate files. The handoff prompt appears inline as a continuation — you **cannot** recover the true LLM conversation history from the main file because the LLM context was actually reset during summarization.

File structure:
```
trajectory.json                              # All main agent steps
trajectory.summarization-1-summary.json      # First summarization: summary subagent
trajectory.summarization-1-questions.json    # First summarization: questions subagent
trajectory.summarization-1-answers.json      # First summarization: answers subagent
trajectory.summarization-2-*.json            # Second summarization (if any)
...
```

**`linear_history=True`:**

Splits the trajectory on summarization boundaries. Each file represents a **continuous, unambiguous linear history** that was actually sent to the LLM.

File structure:
```
trajectory.json                              # Before first summarization
trajectory.cont-1.json                       # After first summarization
trajectory.cont-2.json                       # After second summarization
trajectory.summarization-1-*.json            # First summarization subagents
trajectory.summarization-2-*.json            # Second summarization subagents
```

**What the conversation history actually looks like across summarization:**

```
Turn 1:
  {"user": "System prompt + Task: Create hello.txt + Terminal state: ..."}
  {"assistant": "{'analysis': '...', 'plan': '...', 'commands': [...]}"}

Turn 2:
  {"user": "System prompt + Task + Terminal state: ..."}
  {"assistant": "{'analysis': '...', ...}"}
  {"user": "New Terminal Output:\nroot@container:/app# ..."}
  {"assistant": "{'analysis': '...', ...}"}

// --- Context Summarization Happens ---
// Chat._messages is reset to: [system_prompt, question_prompt, model_questions]
// Next prompt to model is: handoff_prompt + terminal_output

Turn 3:
  {"user": "System prompt + Task + Terminal state: ..."}
  {"user": "You are picking up work from a previous AI agent... Summary: ... Questions: ..."}
  {"assistant": "1. What files have been created so far?\n2. ..."}
  {"user": "Here are the answers...\n\nContinue working..."}
  {"assistant": "{'analysis': '...', ...}"}
```

With `linear_history=True`, the split happens at the summarization boundary, giving you clean input/output sequences suitable for SFT training.

#### Common Configurations

| Use Case | `raw_content` | `linear_history` |
|----------|---------------|------------------|
| Debugging / analysis | `False` | `False` |
| SFT data export | `True` | `True` |
| RL training | `False` | `False` |

---

## Full Source Reference

### Imports

```python
from dataclasses import dataclass
from pathlib import Path

from litellm.utils import get_max_tokens
from tenacity import retry, stop_after_attempt

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.agents.terminus_2.terminus_json_plain_parser import (
    TerminusJSONPlainParser,
)
from terminal_bench.agents.terminus_2.terminus_xml_plain_parser import (
    TerminusXMLPlainParser,
)
from terminal_bench.llms.base_llm import (
    ContextLengthExceededError,
    OutputLengthExceededError,
)
from terminal_bench.llms.chat import Chat
from terminal_bench.llms.lite_llm import LiteLLM
from terminal_bench.terminal.tmux_session import TmuxSession
from terminal_bench.utils.logger import logger
```

### Key Constants & Defaults

| Constant | Value | Location |
|----------|-------|----------|
| Default max episodes | `1,000,000` | `__init__` (when `max_episodes=None`) |
| Default temperature | `0.7` | `__init__` |
| Max command duration | `60` seconds | `_handle_llm_interaction` |
| Proactive summary threshold | `8000` tokens | `_run_agent_loop` (hardcoded) |
| Unwind target free tokens | `4000` tokens | `_unwind_messages_to_free_tokens` |
| Max terminal output bytes | `10000` bytes | `_limit_output_length` |
| LLM retry attempts | `3` | `@retry` decorator on `_query_llm` |
| Context limit fallback | `1,000,000` tokens | `_get_model_context_limit` |
| Output length error token limit | `4096` tokens | Error message in `_query_llm` |

### External Dependencies

| Dependency | Purpose |
|------------|---------|
| `litellm` | LLM API calls (`utils.get_max_tokens`, `utils.token_counter`) |
| `tenacity` | Retry decorator for `_query_llm` |
| `terminal_bench` | Harbor's terminal benchmark framework (agent base class, LLM wrappers, tmux session, parsers) |

---

## Usage Examples

### Via Harbor CLI

```bash
harbor run \
  --agent terminus-2 \
  --model openai/gpt-5 \
  --path examples/tasks/ \
  --task-name hello-world
```

### Via Python (Harbor AgentConfig)

```python
from harbor.models.trial.config import AgentConfig
from harbor.models.agent_name import AgentName

agent_config = AgentConfig(
    name=AgentName.TERMINUS_2,
    model_name="openai/gpt-5",
    kwargs={
        "parser_name": "json",
        "temperature": 0.7,
        "max_turns": 100,
        "enable_summarize": True,
        "proactive_summarization_threshold": 8000,
    }
)
```

### Via Python (Direct Instantiation)

```python
from harbor.agents.terminus_2 import Terminus2
from harbor.models.agent.trajectory_config import TrajectoryConfig

trajectory_config = TrajectoryConfig(
    raw_content=True,       # Preserve exact LLM responses for SFT
    linear_history=True     # Split on summarization for clean sequences
)

agent = Terminus2(
    logs_dir=Path("logs"),
    model_name="anthropic/claude-3-5-sonnet-20241022",
    trajectory_config=trajectory_config,
    parser_name="xml",
    temperature=0.3,
)
```

---

## Summary of Design Decisions

| Decision | Rationale |
|----------|-----------|
| Single tmux tool instead of many tools | Universal interface; works with any TUI application; avoids tool definition overhead |
| Separate agent process from container | Safety isolation; remote execution; clean architecture |
| Raw keystrokes, not structured commands | Enables vim, less, SSH, python repl, arrow-key navigation |
| Double-confirmation for task completion | Prevents premature termination; lets model see final state |
| 3-step summarization (summary → questions → answers) | Higher quality summaries than single-pass; questions fill gaps the summarizer missed |
| Byte-accurate output truncation (first+last halves) | Critical info often at both top (command start) and bottom (prompt line) |
| Manual message injection on output errors | Model needs to see truncated output to correct it; Chat wrapper doesn't append on error |
| XML parser salvage for truncated output | XML is parsable even when truncated mid-document; valid elements can be extracted |
| Unwinding before summarization | Ensures enough context space for the summarization LLM calls themselves |
