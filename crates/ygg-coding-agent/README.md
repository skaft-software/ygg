# ygg-coding-agent

The `ygg` terminal coding agent. It supports interactive, chronological plain,
and response-only print modes; local OpenAI-compatible endpoints; major cloud
providers; branchable persistent sessions; bounded tools; context compaction;
and explicit workspace trust/tool policies.

The customization layer is deliberately local and inspectable: drop prompt
templates, skills, themes, or executable extensions into a project `.ygg/`
directory, then inspect or reload them without rebuilding the binary. See the
[resource contract](../../docs/resources.md), [extension API](../../docs/extensions.md),
[theme system](../../docs/themes.md), [session tools](../../docs/sessions.md), and
[examples](../../examples/README.md).

See the [workspace README](https://github.com/skaft-software/ygg#readme) for
installation, provider setup, safety defaults, and release status.
