import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleStop,
  ExternalLink,
  FileDiff,
  Folder,
  HelpCircle,
  ShieldCheck,
  ShieldQuestion,
  TerminalSquare,
} from "lucide-react";
import type {
  ActionItem,
  OutputRef,
  ReportedTestCounts,
  RunOutcomeItem,
} from "../protocol";

interface CompletionReviewProps {
  outcome: RunOutcomeItem;
  actions: readonly ActionItem[];
  outputs: ReadonlyMap<string, OutputRef>;
  onOpenOutput: (outputId: string) => void;
  onOpenResource?: (
    handle: string,
    title: string,
    presentation: "text" | "diff" | "image",
  ) => void;
  compact?: boolean;
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1_000) return `${durationMs}ms`;
  const seconds = Math.round(durationMs / 1_000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}

const phaseLabels = {
  investigated: "Investigated",
  changed: "Changed",
  verified: "Verified",
  produced: "Produced",
  other: "Other",
} as const;

const frameworkLabels = {
  cargoLibtest: "Cargo tests",
  vitest: "Vitest",
  jest: "Jest",
  pytest: "Pytest",
  goTest: "Go tests",
} as const;

function reportedCountsText(counts: ReportedTestCounts): string | null {
  const facts = [
    counts.total === undefined ? null : `${counts.total} reported`,
    counts.passed === undefined ? null : `${counts.passed} passed`,
    counts.failed === undefined ? null : `${counts.failed} failed`,
    counts.skipped === undefined ? null : `${counts.skipped} skipped`,
    counts.errors === undefined ? null : `${counts.errors} errors`,
  ].filter((fact): fact is string => Boolean(fact));
  return facts.length ? facts.join(" · ") : null;
}

interface ChangeTreeNode {
  name: string;
  path: string;
  action?: ActionItem;
  children: ChangeTreeNode[];
}

interface MutableChangeTreeNode {
  name: string;
  path: string;
  action?: ActionItem;
  children: Map<string, MutableChangeTreeNode>;
}

function changeTree(actions: readonly ActionItem[]): ChangeTreeNode[] {
  const root = new Map<string, MutableChangeTreeNode>();
  for (const action of actions) {
    const displayPaths = action.changedPaths.length
      ? action.changedPaths
      : [action.target ?? action.label];
    for (const displayPath of new Set(displayPaths)) {
      const segments = displayPath.split("/").filter(Boolean);
      if (!segments.length) segments.push(displayPath);
      let children = root;
      let path = "";
      for (const [index, segment] of segments.entries()) {
        path = path ? `${path}/${segment}` : segment;
        let node = children.get(segment);
        if (!node) {
          node = { name: segment, path, children: new Map() };
          children.set(segment, node);
        }
        if (index === segments.length - 1) node.action = action;
        children = node.children;
      }
    }
  }
  const materialize = (
    nodes: ReadonlyMap<string, MutableChangeTreeNode>,
  ): ChangeTreeNode[] =>
    Array.from(nodes.values())
      .map((node) => ({
        name: node.name,
        path: node.path,
        action: node.action,
        children: materialize(node.children),
      }))
      .sort((left, right) => {
        const leftFolder = left.children.length > 0;
        const rightFolder = right.children.length > 0;
        return (
          Number(rightFolder) - Number(leftFolder) ||
          left.name.localeCompare(right.name)
        );
      });
  return materialize(root);
}

function ChangeTree({
  nodes,
  onOpenResource,
}: {
  nodes: readonly ChangeTreeNode[];
  onOpenResource: CompletionReviewProps["onOpenResource"];
}) {
  return (
    <div role="tree" className="completion-review-change-tree">
      {nodes.map((node) => {
        const action = node.action;
        const canOpen = Boolean(action?.diffHandle && onOpenResource);
        return (
          <div role="treeitem" aria-label={node.path} key={node.path}>
            {node.children.length ? (
              <span className="completion-review-tree-folder">
                <Folder aria-hidden="true" />
                <strong>{node.name}</strong>
              </span>
            ) : (
              <button
                type="button"
                disabled={!canOpen}
                onClick={() => {
                  if (!action?.diffHandle || !onOpenResource) return;
                  onOpenResource(
                    action.diffHandle,
                    `${node.path} changes`,
                    "diff",
                  );
                }}
              >
                <FileDiff aria-hidden="true" />
                <span>
                  <strong>{node.name}</strong>
                  {typeof action?.additions === "number" ? (
                    <small>
                      +{action.additions} −{action.deletions ?? 0}
                    </small>
                  ) : (
                    <small>{action?.summary ?? "Changed"}</small>
                  )}
                </span>
                {canOpen ? <ChevronRight aria-hidden="true" /> : null}
              </button>
            )}
            {node.children.length ? (
              <div role="group">
                <ChangeTree
                  nodes={node.children}
                  onOpenResource={onOpenResource}
                />
              </div>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}

export function CompletionReview({
  outcome,
  actions,
  outputs,
  onOpenOutput,
  onOpenResource,
  compact = false,
}: CompletionReviewProps) {
  const { review } = outcome;
  const actionById = new Map(actions.map((action) => [action.id, action]));
  const changed = review.changedFileItemIds
    .map((id) => actionById.get(id))
    .filter((action): action is ActionItem => Boolean(action));
  const verification = review.verificationActionItemIds
    .map((id) => actionById.get(id))
    .filter((action): action is ActionItem => Boolean(action));
  const failures = review.failedActionItemIds
    .map((id) => actionById.get(id))
    .filter((action): action is ActionItem => Boolean(action));
  const failureIds = new Set(failures.map((action) => action.id));
  const warnings = review.warningActionItemIds
    .map((id) => actionById.get(id))
    .filter(
      (action): action is ActionItem =>
        Boolean(action) && !failureIds.has(action!.id),
    );
  const reviewOutputs = review.outputIds
    .map((id) => outputs.get(id))
    .filter((output): output is OutputRef => Boolean(output));
  const duration = review.durationMs || outcome.durationMs;

  return (
    <section
      className={`completion-review is-${outcome.outcome} ${compact ? "is-compact" : ""}`}
      aria-label="Completion review"
    >
      <header className="completion-review-header">
        <span className="completion-review-status" aria-hidden="true">
          {outcome.outcome === "done" ? (
            <Check />
          ) : outcome.outcome === "failed" ? (
            <AlertTriangle />
          ) : (
            <CircleStop />
          )}
        </span>
        <span>
          <strong>
            {outcome.outcome === "done"
              ? "Ready for review"
              : outcome.outcome === "failed"
                ? "Run failed"
                : "Run stopped"}
          </strong>
          <small>{review.summary || outcome.summary}</small>
        </span>
        {duration > 0 ? <time>{formatDuration(duration)}</time> : null}
      </header>

      <div className="completion-review-facts" aria-label="Review summary">
        <span>
          <strong>{review.actionCount}</strong>
          <small>{review.actionCount === 1 ? "action" : "actions"}</small>
        </span>
        <span>
          <strong>{changed.length}</strong>
          <small>{changed.length === 1 ? "changed file" : "changed files"}</small>
        </span>
        <span>
          <strong>{verification.length}</strong>
          <small>verification</small>
        </span>
        <span data-coverage={review.evidenceCoverage}>
          {review.evidenceCoverage === "complete" ? (
            <ShieldCheck aria-hidden="true" />
          ) : (
            <ShieldQuestion aria-hidden="true" />
          )}
          <small>{review.evidenceCoverage} evidence</small>
        </span>
      </div>

      {review.phases.length ? (
        <div className="completion-review-phases" aria-label="Activity phases">
          {review.phases.map((phase) => (
            <span key={phase.phase} data-failed={phase.failedCount > 0}>
              {phaseLabels[phase.phase]}
              <b>{phase.actionCount}</b>
            </span>
          ))}
        </div>
      ) : null}

      {changed.length ? (
        <section className="completion-review-section">
          <h4>Changed files</h4>
          <ChangeTree
            nodes={changeTree(changed)}
            onOpenResource={onOpenResource}
          />
        </section>
      ) : null}

      {verification.length ? (
        <section className="completion-review-section">
          <h4>Verification</h4>
          <div className="completion-review-list">
            {verification.map((action) => (
              <div key={action.id} className="completion-review-row">
                <TerminalSquare aria-hidden="true" />
                <span>
                  <strong>{action.label}</strong>
                  <small>
                    {action.outputSummary ??
                      action.summary ??
                      action.commandPreview ??
                      "Verification completed"}
                  </small>
                </span>
                <em data-status={action.status}>
                  {typeof action.exitCode === "number"
                    ? `exit ${action.exitCode}`
                    : action.status}
                </em>
              </div>
            ))}
          </div>
        </section>
      ) : null}

      {review.testResults.length ? (
        <section className="completion-review-section is-tests">
          <h4>Test results</h4>
          <div className="completion-review-test-results">
            {review.testResults.map((result) => {
              const counts = reportedCountsText(result.reported);
              const partial =
                result.verification === "inconclusive" ||
                result.coverage.inputTruncated ||
                result.coverage.recordsTruncated ||
                result.coverage.unsupportedSummaryFields;
              return (
                <details
                  key={result.originItemId}
                  data-verification={result.verification}
                >
                  <summary>
                    <span>
                      <strong>{frameworkLabels[result.framework]}</strong>
                      <small>
                        {counts ??
                          (partial
                            ? "Reporter evidence is incomplete"
                            : "Reporter did not publish aggregate counts")}
                      </small>
                    </span>
                    <em>{result.verification}</em>
                  </summary>
                  {result.suites.length ? (
                    <div className="completion-review-test-suites">
                      {result.suites.map((suite, index) => (
                        <div key={`${suite.name}-${index}`}>
                          <span>
                            <strong>{suite.name}</strong>
                            <small>
                              {reportedCountsText(suite.reported) ??
                                (suite.status
                                  ? `Suite ${suite.status}`
                                  : "No suite aggregate reported")}
                            </small>
                          </span>
                          {suite.cases.length ? (
                            <ul>
                              {suite.cases.map((testCase, caseIndex) => (
                                <li key={`${testCase.name}-${caseIndex}`}>
                                  <span>{testCase.name}</span>
                                  <em data-status={testCase.status}>
                                    {testCase.status}
                                  </em>
                                </li>
                              ))}
                            </ul>
                          ) : null}
                        </div>
                      ))}
                    </div>
                  ) : null}
                  {partial ? (
                    <p>
                      Counts are shown only where the supported reporter proved
                      them; omitted or truncated evidence is not inferred.
                    </p>
                  ) : null}
                </details>
              );
            })}
          </div>
        </section>
      ) : null}

      {failures.length || warnings.length ? (
        <section className="completion-review-section is-attention">
          <h4>Failures and warnings</h4>
          <div className="completion-review-list">
            {[...failures, ...warnings].map((action) => (
              <div key={action.id} className="completion-review-row">
                <AlertTriangle aria-hidden="true" />
                <span>
                  <strong>{action.label}</strong>
                  <small>
                    {action.outputSummary ??
                      action.summary ??
                      "Needs attention"}
                  </small>
                </span>
              </div>
            ))}
          </div>
        </section>
      ) : null}

      {reviewOutputs.length ? (
        <section className="completion-review-section">
          <h4>Outputs</h4>
          <div className="completion-review-list">
            {reviewOutputs.map((output) => (
              <button
                key={output.id}
                type="button"
                onClick={() => onOpenOutput(output.id)}
              >
                <ExternalLink aria-hidden="true" />
                <span>
                  <strong>{output.title}</strong>
                  <small>{output.subtitle}</small>
                </span>
                <ChevronRight aria-hidden="true" />
              </button>
            ))}
          </div>
        </section>
      ) : null}

      {review.openQuestions.length ? (
        <section className="completion-review-section is-questions">
          <h4>Open questions</h4>
          <ul>
            {review.openQuestions.map((question) => (
              <li key={question}>
                <HelpCircle aria-hidden="true" />
                <span>{question}</span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </section>
  );
}
