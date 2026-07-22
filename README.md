# Ygg

Ygg is a local-first coding agent for macOS and Linux. It provides an interactive terminal UI, plain and print modes, persistent branchable sessions, tool execution, automatic context compaction, and one provider-independent conversation model across OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages.

> **Release status:** `0.1.0-alpha`. The core safety and persistence invariants are regression-tested, but the product is evolving quickly. Ygg is not an operating-system sandbox.

## Install

Rust 1.86 or newer and `rg` (ripgrep) are required. On macOS or Linux, the
installer builds the pinned release and adds Cargo's bin directory to the
startup file for zsh, bash, or POSIX sh when it is not already on `PATH`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/skaft-software/ygg/v0.1.0-alpha/scripts/install.sh | sh
```

Restart the shell after installation, then run `ygg --version`. To install
without changing a shell startup file:

```sh
cargo install --locked --git https://github.com/skaft-software/ygg --tag v0.1.0-alpha --bin ygg ygg-coding-agent
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
```

From a checkout:

```sh
cargo install --locked --path crates/ygg-coding-agent --bin ygg
ygg --version
```

During development, install the debug-profile candidate without producing a release build:

```sh
cargo install --debug --locked --path crates/ygg-coding-agent --bin ygg
```

If Rust is not installed yet, install it from [rustup.rs](https://rustup.rs/)
and rerun the Ygg installer.

## Quick start

### Cloud model

Set a provider credential and select a model:

```sh
export ANTHROPIC_API_KEY='...'
ygg --model claude-sonnet-4-6

# Other first-class examples
export OPENAI_API_KEY='...'
ygg --model gpt-5.4

export OPENROUTER_API_KEY='...'
ygg --model openrouter/anthropic/claude-sonnet-4.6
```

Ygg also supports DeepSeek, Groq, Cerebras, xAI, Together, Fireworks, NVIDIA, Hugging Face, Moonshot, MiniMax, OpenCode, and ChatGPT subscription credentials (`ygg --login codex`). Run `ygg --help` for launch controls and use `/model` in the TUI.

### Local OpenAI-compatible model

Create `~/.ygg/credentials/custom.json` with owner-only permissions:

```json
{
  "base_url": "http://127.0.0.1:8000/v1/",
  "api_key": "",
  "auto_discover": false,
  "models": [
    {
      "api_name": "Qwen/Qwen3-Coder-Next",
      "display_name": "Qwen3 Coder Next",
      "context_window": 131072,
      "max_output_tokens": 16384,
      "tools": true,
      "parallel_tool_calls": false,
      "vision": false,
      "structured_output": false,
      "reasoning": true
    }
  ]
}
```

Then run:

```sh
chmod 600 ~/.ygg/credentials/custom.json
ygg --offline --model 'custom/Qwen3 Coder Next'
```

`--offline` skips optional startup model discovery; inference still reaches the selected endpoint.

## Safety defaults

- Explicit built-in file paths are workspace-only by default.
- Project `.ygg/config.toml`, workspace `AGENTS.md`, and workspace skills are ignored unless `--workspace-trusted` is supplied.
- A trusted project may tighten global capability/resource settings but cannot relax them.
- `AGENTS.md`, config, credential, discovery, session, and provider-stream inputs have hard bounds; context files must be regular non-symlink files.
- `--no-edit` disables both `edit` and `write`.
- The default work surface is `read`, `edit`, `write`, and `exec`; `search`
  remains available through an explicit `--tools` allowlist.
- `--tools read,search`, `--exclude-tools`, and `--no-tools` control both provider schemas and executable implementations.
- Mutating calls left unresolved by a crash are reported as indeterminate and are never automatically replayed.
- Session files/directories are private and writes are locked and synced.

For a read-only review:

```sh
ygg --tools read,search --no-context-files --offline
```

Command execution has the full authority of the current user. `allow_process` and `allow_shell` form one unified command gate because invoking an arbitrary interpreter directly is shell-equivalent. See [SECURITY.md](SECURITY.md).

## Configuration

Global settings live at `~/.ygg/config.toml`. Trusted project settings may live at `.ygg/config.toml`. Environment variables and explicit CLI flags have higher precedence.

Useful controls:

```text
--workspace PATH
--workspace-trusted
--model MODEL
--reasoning off|minimal|low|medium|high|xhigh|max|budget=N
--tools NAMES
--exclude-tools NAMES
--no-tools
--no-edit
--no-write
--no-process / --no-shell
--no-context-files
--offline
--print PROMPT
--plain
--continue / --resume [ID]
```

## Development and release gates

Normal builds are deterministic and do not download model metadata.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --locked
cargo check --workspace --all-targets --all-features --locked
cargo package --workspace --locked --no-verify
```

The declared MSRV is Rust 1.86. CI covers Linux and macOS; the alpha command-execution implementation is Unix-only.

## Documentation

- [Security policy and containment guidance](SECURITY.md)
- [Changelog](CHANGELOG.md)
- [Customization resource discovery and trust](docs/resources.md)
- [Executable extensions and typed contributions](docs/extensions.md)
- [Themes and semantic terminal styling](docs/themes.md)
- [Session inspection, export, and repair](docs/sessions.md)
- [Prompt, skill, theme, and extension examples](examples/README.md)
- [Agent architecture](docs/design/ygg-agent.md)
- [Coding-agent product contract](docs/design/ygg-coding-agent.md)
- [Provider architecture](docs/design/ygg-ai.md)

Licensed under either Apache-2.0 or MIT at your option.
