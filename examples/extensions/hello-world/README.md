# hello-world executable extension

This dependency-free Python process demonstrates Ygg's `0.1` JSON-lines
extension protocol: initialization, a custom model tool, a slash command,
lifecycle hooks, prompt context, a semantic status contribution, a tool
renderer, and a notification.

Copy this directory to `.ygg/extensions/hello-world/`, explicitly enable and
trust `hello-world`, then restart or reload extensions. Ygg resolves the bare
`extension.py` entrypoint beside `extension.toml` and launches it directly;
Python 3 must be available through the shebang environment.

Stdout stays protocol-only. Add ordinary diagnostics on stderr when adapting
this example to another language.
