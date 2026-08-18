# git-tools executable extension

This Python example uses the dependency-free `ygg-extension-sdk` package and
contributes three related pieces through the Ygg `0.1` stdio protocol:

- `git_status`, a model tool with bounded arguments, a five-second timeout,
  bounded output, and structured metadata;
- `/checkpoint [label]`, a deliberately read-only checkpoint preview; and
- a semantic `git_status` renderer that returns theme roles rather than ANSI
  escape sequences.

Install the SDK before copying the directory:

```console
python3 -m pip install ./sdk/python
```

Copy the directory to `.ygg/extensions/git-tools/`, then explicitly enable and
trust `git-tools`. The default full-access mode launches it; `--safe-mode` keeps
a stricter admission profile and will not launch it. Git must be on `PATH`. The
extension runs only read commands, sets `GIT_OPTIONAL_LOCKS=0`, never invokes a
shell, and does not create commits.

Its declared `process = true` capability is visible consent metadata for
launching `git status`; it is not an operating-system sandbox.
