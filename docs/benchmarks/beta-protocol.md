# Ten-user daily-driver beta protocol

This is an opt-in, no-telemetry-required protocol for the first ten developers.
The purpose is to find reasons a capable developer abandons Ygg before it earns
repeated use.

## Setup

Give each participant the same short checklist and no author assistance unless
they are blocked for 15 minutes:

1. Install from the pinned release instructions or build from source.
2. Configure either a local OpenAI-compatible endpoint or a cloud provider.
3. Run `ygg --help`, start one session, and complete a small repository task.
4. Exit, resume with `ygg --continue`, and complete a second task.
5. Cancel one intentionally long-running operation and regain the prompt.
6. Inspect `/status` (or the equivalent status command) and report the active
   model, endpoint class, and context information.

Do not ask participants to enable telemetry.  If they volunteer diagnostics,
`ygg --telemetry ./ygg-telemetry.jsonl` produces a redacted operational trace;
participants should inspect it before sharing.

## Per-participant record

Collect only through an issue template, interview, or an exported local form:

- OS, CPU/RAM/GPU, Ygg version, install method, and competitor normally used
- provider class (`local`, `remote`, or `subscription`), not credentials
- installed successfully without author help: yes/no and minutes
- first task completed: yes/no and minutes
- resume/cancel behavior: pass/fail
- crashes, hangs, provider configuration failures, and abandoned tasks
- days active and sessions completed over 14 days
- which existing agent they returned to, if any, and why
- one thing Ygg did better and one thing it did worse
- whether they would notice if Ygg disappeared

Never request API keys, raw prompts, private repositories, unredacted session
files, or mandatory background telemetry.

## Success thresholds

The beta is evidence of onboarding quality only if at least 8/10 participants
install and complete a first task without author intervention, at least 6/10
return for a second day, and no unresolved crash or data-loss issue is hidden by
an average. Publish the numerator, denominator, exclusions, and reason
categories; do not turn a small beta into a superiority claim.

## Diagnostic bundle rules

A shareable bundle may contain version/build identity, platform, provider kind,
configuration keys with values removed, sanitized startup diagnostics, and
explicitly selected telemetry. It must exclude credentials, authorization
headers, raw prompts, tool arguments/results, workspace paths where possible,
and session content unless the participant deliberately redacts and approves
it. The bundle command should be an export convenience, not a prerequisite for
using Ygg.
