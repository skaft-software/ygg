# local-model-workflow executable extension

This Python example uses the dependency-free `ygg-extension-sdk` package to
show how a local-model workflow can remain explicit and inspectable instead of
silently rewriting prompts. It contributes:

- a `before_prompt` hook that returns compact, labeled system-suffix context;
- the same deterministic text through `context/collect` for normal prompt
  composition and context inspection;
- a semantic status item derived from current model and active-skill metadata;
  and
- one process-originated notification when prompt shaping first becomes active.

Install the SDK before copying the directory:

```console
python3 -m pip install ./sdk/python
```

Copy the directory to `.ygg/extensions/local-model-workflow/`, explicitly
enable and trust it, and opt into UnsafeHost (`--yolo`) before restarting Ygg
or using `/extensions reload`. `--safe` discovers the manifest but never starts
its process. The existing frontend integration exposes its
typed hook, context, status, and event contributions. The extension itself
reads no files, launches no child subprocesses, accesses no network, and uses no
terminal escape sequences.

The workflow context is intentionally short for small context windows. It is
deterministic for the same host state and exposes its label and placement so a
user can see exactly what will reach the model.
