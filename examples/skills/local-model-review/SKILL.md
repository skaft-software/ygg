---
name: Local Model Review
description: Run a focused, evidence-first code review without flooding a small context window.
version: 0.1.0
required-tools:
  - read
  - search
tags:
  - review
  - local-model
---
# Local Model Review

Use this procedure only after explicit activation.

1. Identify the requested change boundary and inspect the relevant diff or files.
2. Trace each changed behavior to its callers and tests. Avoid loading unrelated directories.
3. Prioritize concrete correctness, security, data-loss, and compatibility risks.
4. For every finding, cite the narrowest useful file location and explain the failing scenario.
5. If there are no findings, say so and name the verification you performed.

Load `references/checklist.md` only when a structured review checklist will help.
