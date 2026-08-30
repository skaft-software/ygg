# Failure taxonomy

This file is intentionally conservative.  It is populated only after reading
the retained per-instance evaluator result and trajectory.  A failed verifier
result alone is not evidence of a model, policy, or harness cause.

Primary categories:

- `model_capability`: issue understanding or implementation is wrong after the
  normal agent loop and environment are shown healthy.
- `agent_policy`: premature completion, insufficient validation, redundant
  actions, or poor sequencing supported by the trajectory.
- `prompt_or_tool_design`: a reproducible limitation in the model-visible
  instructions/tool contract, separate from the model's code reasoning.
- `runtime_harness`: process cancellation, diff capture, state isolation,
  telemetry, or lifecycle defect reproduced outside the issue solution.
- `provider_failure`: authentication, quota, transport, or provider response
  failure supported by stderr/telemetry.
- `environment_or_evaluator`: image, dependency, network fixture, flaky test,
  missing report, or official evaluator problem supported by independent checks.
- `verifier_negative`: official evaluator rejected the submitted patch but the
  trajectory does not justify a narrower cause.

For each classified task record the evidence paths, confidence (`high`,
`medium`, or `low`), and any competing explanation.  Do not classify a task as
`runtime_harness` merely because Ygg failed to solve it.

## Agent-loop efficiency review

The aggregate report includes trace-derived, non-causal indicators for:

- assistant turns with more than one tool call and interval-derived maximum
  concurrent tools (independent fan-out);
- compound `bash` commands containing `&&`, `||`, or `;` (dependent-shell
  batching signal);
- repeated calls and no-progress metadata emitted by Ygg telemetry;
- request elapsed time, tool elapsed time, and residual agent time.

These are observations for a post-baseline optimization pass.  They must not be
used to alter the frozen pilot or to retroactively improve its score.
