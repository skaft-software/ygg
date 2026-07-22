---
name: Repro First
description: Diagnose a runtime failure from a minimal reproduction before proposing a fix.
version: 0.1.0
required-tools:
  - read
  - search
  - exec
tags:
  - debugging
  - diagnostics
---
# Repro First

1. Capture the exact command, input, observed output, and expected output.
2. Reduce the failure to the smallest safe deterministic reproduction.
3. Trace the actual lifecycle or data path; do not treat the final error message as the root cause.
4. Form one falsifiable hypothesis at a time and test it.
5. Preserve useful diagnostics in any eventual fix and add a regression test around the reproduction.

Do not mutate files unless the user has asked for a fix as well as a diagnosis.
