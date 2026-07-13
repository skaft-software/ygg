# Prompt for the `ygg-ai` implementing agent

Copy everything below into a fresh coding-agent session started at the repository root.

---

You are the primary implementation owner for `ygg-ai`. Build the complete package end to end. This is not a planning exercise and not a partial/demo implementation.

## Authority and required reading

Before editing anything:

1. Locate the repository root and read these files **completely**, continuing across tool-output truncation until every line has been read:
   - `docs/design/ygg-ai.md`
   - `docs/plans/ygg-ai-implementation.md`
2. Read the repository API documentation under `docs/research/apidocs/` whenever implementing a protocol DTO or fixture. The design defines canonical behavior; repository API docs define wire fields.
3. Inspect the existing repository/workspace before creating files. Preserve established conventions where they do not conflict with the normative design.

The precedence rules in `docs/design/ygg-ai.md` are binding. Do not invent architecture or wire behavior. If something appears contradictory, first apply those precedence rules and search the repository docs. If a genuine contradiction remains and prevents a correct implementation, document the exact conflict in the report rather than guessing—but continue all unblocked work.

## Mission

Implement **all tasks 1.1 through 16.2** in `docs/plans/ygg-ai-implementation.md`, including all three protocols, all canonical types, validation, strict/lossy behavior, auth, catalog, pricing, SSE decoding, streaming assembly, client dispatch, complete(), fixtures, cross-protocol replay, public docs, hardening, and the final adversarial audit.

There is no slice, MVP, or sanctioned partial target. Do not return merely with a plan, scaffolding, or suggested next steps. Continue until the entire completion gate is green or a genuine external blocker makes further progress impossible.

## Operating rules

- Work in dependency order and keep the crate compiling after each task.
- Create and continuously maintain `docs/reports/ygg-ai-implementation-report.md` as an evidence ledger. For each task record status, files changed, tests added, command run, and result.
- Run every task’s acceptance command before marking it complete.
- Use private serde DTOs per protocol. Never serialize canonical types directly to provider JSON.
- Never guess a provider field. Cite the relevant `docs/research/apidocs/...` file and heading in each fixture’s header comment.
- Do not silently drop data. Strict returns the specified structured error; Lossy performs only the specified derived-wire conversion and emits the exact diagnostic.
- Never mutate canonical request history during conversion.
- Keep secrets out of Debug, Display, serialization, errors, test output, and reports. Mark secret `HeaderValue`s sensitive. Never use a real credential.
- Tests must be deterministic and offline. Do not contact live providers or require API keys.
- Do not add automatic retries or background stream-reader tasks.
- Do not expose protocol DTOs publicly.
- Do not introduce `todo!()`, `unimplemented!()`, placeholder behavior, ignored tests, broad `allow` attributes, weakened assertions, fake provider events, or speculative catalog data.
- Do not modify files under `docs/research/apidocs/`.
- Do not claim success based only on compilation. Behavior and fixture coverage are required.
- If context becomes tight, update the report and a concise working checklist before continuing; do not discard unfinished tasks.

## Mandatory self-review and fix loop

After implementing all phases:

1. Run the full Task 16.1 gate exactly:

   ```sh
   cargo fmt --check && \
   cargo clippy --workspace --all-features --all-targets -- -D warnings && \
   cargo test --workspace --all-features && \
   cargo doc --workspace --no-deps
   ```

2. Perform Task 16.2’s adversarial traceability audit. Walk every public API item and every design table row—not a sample. Link each requirement to source and test evidence in the report.
3. Search the tree for unfinished or suspicious code, including `todo!`, `unimplemented!`, `TODO`, `FIXME`, ignored tests, protocol DTO visibility, debug statements, secret-like fixture values, floating-point pricing arithmetic, forbidden audio event fixtures, and unbounded body accumulation.
4. Inspect `cargo tree -p ygg-ai` for unexpected provider SDKs, default TLS, or other forbidden dependencies.
5. Fix every issue found. Run focused tests after each fix, then rerun the complete gate. Repeat audit/fix/gate until there are no known deviations.
6. Leave the working tree formatted and all generated evidence/report files saved.

## Final response

Your final response must be concise and must point the reviewer to `docs/reports/ygg-ai-implementation-report.md`. Include:

- implementation status (complete or incomplete);
- the exact final gate result;
- a short list of major delivered components;
- any unresolved blocker or spec deviation (say “none” only if the audit supports it);
- files/areas the reviewer should inspect first.

Do not paste a giant narrative into chat; put complete evidence, matrices, command results, and reviewer instructions in the report. Do not call the implementation complete if any mandatory task, test matrix row, audit item, or final command is outstanding.
