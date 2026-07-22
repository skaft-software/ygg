# git-tools executable extension

This Python-stdlib-only example contributes three related pieces through the
Ygg `0.1` stdio protocol:

- `git_status`, a model tool with bounded arguments, a five-second timeout,
  bounded output, and structured metadata;
- `/checkpoint [label]`, a deliberately read-only checkpoint preview; and
- a semantic `git_status` renderer that returns theme roles rather than ANSI
  escape sequences.

Copy the directory to `.ygg/extensions/git-tools/`, then explicitly enable and
trust `git-tools`. Git must be on `PATH`. The extension runs only read commands,
sets `GIT_OPTIONAL_LOCKS=0`, never invokes a shell, and does not create commits.

Its declared `process = true` capability is visible consent metadata for
launching `git status`; it is not an operating-system sandbox.
