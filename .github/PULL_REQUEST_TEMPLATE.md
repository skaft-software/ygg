## Summary

<!-- What changed? Keep this factual and bounded. -->

## User or developer impact

<!-- What observable problem or maintenance burden does this solve? -->

## Design and ownership

- Issue / Discussion / roadmap item:
- Owning boundary (core, extension, provider data/runtime, TUI, Serve, release/docs):
- Why the change does not belong in a smaller or external boundary:
- Compatibility or migration effect:

## Risk

- Authority/security/privacy impact:
- Persistence/data-loss impact:
- Cancellation/retry/ambiguous-effect impact:
- Performance/resource impact:
- Rollback or disable path:

## Verification

<!-- List only checks actually run, with observed results. -->

- [ ] Focused regression
- [ ] Relevant crate/package tests
- [ ] Formatting and lint
- [ ] Documentation/protocol fixtures updated where applicable
- [ ] Full release gate, or an explicit reason it was not run

## Repository hygiene

- [ ] No credentials, sessions, private paths, raw benchmark homes/caches, local notes, or unrelated generated files
- [ ] No new package/provider-name privilege in a generic core path
- [ ] Generated/vendored output has a documented source and regeneration check
- [ ] A touched god object became smaller or delegated to a cohesive owner, or the exception is explained
- [ ] Security-sensitive findings use private reporting rather than this PR

## AI assistance

<!-- Follow CONTRIBUTING.md: disclose material AI assistance and provide inspected prompt context when useful, with private information removed. -->
