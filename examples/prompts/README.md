# Prompt template examples

Copy these files into `.ygg/prompts/` for one trusted project or `~/.ygg/prompts/` globally. Markdown templates intentionally use Pi-compatible frontmatter and argument expansion. Ygg also accepts the small TOML form and adds deterministic variables such as `{{prompt}}` and `{{workspace}}`.

Invoke a discovered template with `/local-review …`, `/prompt local-review …`, or select one at startup with `ygg --prompt local-review "…"`.

Add `--debug-prompt` to display the complete deterministic expansion and its
content hash before the request is sent.
