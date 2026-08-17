# Ygg customization examples

These examples are small, copyable starting points for a local Ygg setup. They
use the same filesystem discovery and typed contribution boundaries as normal
user resources.

The Python extension examples use the dependency-free `ygg-extension-sdk`.
Install it with `python3 -m pip install ./sdk/python` before copying an
extension into a project.

- [`prompts/`](prompts/) — Pi-compatible Markdown prompts and Ygg's compact
  TOML form.
- [`skills/`](skills/) — explicit, inspectable skills with bounded text
  resources.
- [Bundled theme pack](../crates/ygg-coding-agent/themes/) — eleven complete TOML
  themes that can also be copied and edited as project themes.
- [`extensions/hello-world/`](extensions/hello-world/) — a minimal executable
  JSON-RPC extension.
- [`extensions/caffeinate/`](extensions/caffeinate/) — a macOS sleep inhibitor
  active while Ygg processes a prompt.
- [`extensions/git-tools/`](extensions/git-tools/) — a custom command, git
  status tool, status-line contribution, and tool renderer.
- [`extensions/local-model-workflow/`](extensions/local-model-workflow/) —
  deterministic prompt/context shaping for smaller local context windows.

Copy an example into the matching `.ygg/` directory in a project. Executable
extensions must also be explicitly enabled, independently trusted, and started
with the UnsafeHost authority opt-in; see
[`docs/extensions.md`](../docs/extensions.md).
