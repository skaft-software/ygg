# local-model-workflow executable extension

This dependency-free example shows how a local-model workflow can remain
explicit and inspectable instead of silently rewriting prompts. It contributes:

- a `before_prompt` hook that returns compact, labeled system-suffix context;
- the same deterministic text through `context/collect` for normal prompt
  composition and context inspection;
- a semantic status item derived from current model and active-skill metadata;
  and
- one process-originated notification when prompt shaping first becomes active.

Copy the directory to `.ygg/extensions/local-model-workflow/`, explicitly
enable and trust it, then restart Ygg or use `/extensions reload`. The existing
frontend integration exposes its typed hook, context, status, and event
contributions. It reads no files, launches no subprocesses, accesses no
network, and uses no terminal escape sequences.

The workflow context is intentionally short for small context windows. It is
deterministic for the same host state and exposes its label and placement so a
user can see exactly what will reach the model.
