# hello-world executable extension

This example uses the dependency-free `ygg-extension-sdk` package to
demonstrate Ygg's `0.1` JSON-lines extension protocol: initialization, a
custom model tool, a slash command, lifecycle hooks, prompt context, a semantic
status contribution, a tool renderer, and a notification.

Install the SDK before copying the example:

```console
python3 -m pip install ./sdk/python
```

Copy this directory to `.ygg/extensions/hello-world/`, explicitly enable and
trust `hello-world`, then restart or reload extensions. Ygg resolves the bare
`extension.py` entrypoint beside `extension.toml` and launches it directly;
Python 3 must be available through the shebang environment.

Stdout stays protocol-only. The SDK sends structured diagnostics to stderr.
