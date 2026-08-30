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

## Open provider-compatibility bug (recorded 2026-08-27)

- OpenAI Responses error events can fail during response decoding when a field in
  the nested `error` object is JSON `null` even though the decoder expects a
  string. Observed: `provider=openai-codex model=gpt-5.6-luna phase=response
  decoding detail=JSON decode error: invalid OpenAI Responses error event
  (error,sequence_number,type; error=code,message,param,type): invalid type:
  null, expected a string`.
- Follow-up: identify the nullable field (likely `error.param`), make the wire
  fixture/parser accept the provider's null shape, and add malformed-event
  regression coverage.
