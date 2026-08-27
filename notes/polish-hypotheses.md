# Polish hypotheses

Backlog of UX polish hypotheses. Each needs evaluation under the broader principle:
**application chrome should move only when the change communicates a state the user can name.**

## Composer rule colors (recorded 2026-08-25)

Hypothesis: composer rules changing color only during typing or model activity may create
ambiguous state motion. The key question is whether they communicate focus/liveness
predictably or merely make the frame flicker.

Options to evaluate:

1. Always stable (no activity-driven color change)
2. Focus-only
3. Run-only
4. Current behavior: typing-or-run

Evaluate alongside the collapsed-thinking fix (model-authored headings promoted into the
collapsed thinking indicator can change too often and feel like random status text).
