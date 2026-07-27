/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  ActionItem,
  CompletionReview as CompletionReviewModel,
  RunOutcomeItem,
  StructuredTestResults,
} from "../protocol";
import { CompletionReview } from "./CompletionReview";

function outcomeWith(
  testResults: StructuredTestResults[],
): RunOutcomeItem {
  const review: CompletionReviewModel = {
    summary: "Verification evidence was collected.",
    durationMs: 120,
    actionCount: 1,
    phases: [],
    changedFileItemIds: [],
    verificationActionItemIds: [],
    failedActionItemIds: [],
    warningActionItemIds: [],
    sourceIds: [],
    outputIds: [],
    testResults,
    evidenceCoverage: "partial",
    openQuestions: [],
  };
  return {
    id: "item-outcome",
    runId: "run-test",
    turnId: "turn-test",
    kind: "run_outcome",
    outcome: "done",
    durationMs: 120,
    summary: review.summary,
    review,
    state: "committed",
    createdAt: new Date(1_753_626_615_000).toISOString(),
  };
}

function result(
  overrides: Partial<StructuredTestResults> = {},
): StructuredTestResults {
  return {
    originItemId: "item-vitest",
    framework: "vitest",
    parser: "vitestTextV1",
    command: { status: "succeeded", exitCode: 0 },
    verification: "inconclusive",
    reported: { total: 5, passed: 4, skipped: 1 },
    reportedSuites: { total: 1, passed: 1 },
    summaryCount: 1,
    suites: [
      {
        name: "src/safe.test.ts",
        reported: { total: 2, passed: 1 },
        cases: [{ name: "keeps evidence bounded", status: "passed" }],
      },
    ],
    coverage: {
      inputTruncated: true,
      recordsTruncated: false,
      unsupportedSummaryFields: false,
      summaries: "partial",
      cases: "partial",
    },
    ...overrides,
  };
}

describe("CompletionReview structured test evidence", () => {
  afterEach(cleanup);

  it("shows only reporter-proved counts and labels incomplete evidence", async () => {
    const user = userEvent.setup();
    render(
      <CompletionReview
        outcome={outcomeWith([result()])}
        actions={[]}
        outputs={new Map()}
        onOpenOutput={vi.fn()}
      />,
    );

    expect(
      screen.getByText("5 reported · 4 passed · 1 skipped"),
    ).toBeVisible();
    expect(screen.queryByText(/1 failed/)).toBeNull();
    expect(screen.queryByText(/0 failed/)).toBeNull();

    const summary = screen.getByText("Vitest").closest("summary");
    expect(summary).not.toBeNull();
    await user.click(summary!);
    expect(
      screen.getByText("2 reported · 1 passed"),
    ).toBeVisible();
    expect(screen.getByText("keeps evidence bounded")).toBeVisible();
    expect(
      screen.getByText(
        /Counts are shown only where the supported reporter proved them/,
      ),
    ).toBeVisible();
  });

  it("renders an explicit zero failure count when the reporter supplied it", () => {
    render(
      <CompletionReview
        outcome={outcomeWith([
          result({
            verification: "passed",
            reported: {
              total: 3,
              passed: 3,
              failed: 0,
              skipped: 0,
              errors: 0,
            },
            coverage: {
              inputTruncated: false,
              recordsTruncated: false,
              unsupportedSummaryFields: false,
              summaries: "complete",
              cases: "complete",
            },
          }),
        ])}
        actions={[]}
        outputs={new Map()}
        onOpenOutput={vi.fn()}
      />,
    );

    expect(
      screen.getByText(
        "3 reported · 3 passed · 0 failed · 0 skipped · 0 errors",
      ),
    ).toBeVisible();
    expect(
      screen.queryByText(/Reporter evidence is incomplete/),
    ).toBeNull();
  });

  it("groups nested changed paths and opens the exact originating diff", async () => {
    const user = userEvent.setup();
    const changed: ActionItem = {
      id: "item-change",
      runId: "run-test",
      turnId: "turn-test",
      kind: "action",
      actionKind: "file_write",
      phase: "changed",
      status: "succeeded",
      rawToolName: "apply_patch",
      label: "Updated web files",
      observedOutputBytes: 128,
      droppedOutputBytes: 0,
      changedPaths: [
        "apps/web/src/App.tsx",
        "apps/web/src/store.ts",
      ],
      sourceIds: [],
      outputIds: [],
      additions: 12,
      deletions: 4,
      diffHandle: "resource-nested-diff",
      state: "committed",
      createdAt: new Date(1_753_626_615_000).toISOString(),
    };
    const outcome = outcomeWith([]);
    outcome.review.changedFileItemIds = [changed.id];
    const onOpenResource = vi.fn();
    render(
      <CompletionReview
        outcome={outcome}
        actions={[changed]}
        outputs={new Map()}
        onOpenOutput={vi.fn()}
        onOpenResource={onOpenResource}
      />,
    );

    expect(screen.getByRole("treeitem", { name: "apps" })).toBeVisible();
    expect(
      screen.getByRole("treeitem", { name: "apps/web/src" }),
    ).toBeVisible();
    expect(
      screen.getByRole("treeitem", {
        name: "apps/web/src/App.tsx",
      }),
    ).toBeVisible();
    expect(
      screen.getByRole("treeitem", {
        name: "apps/web/src/store.ts",
      }),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: /App\.tsx/ }));
    expect(onOpenResource).toHaveBeenCalledWith(
      "resource-nested-diff",
      "apps/web/src/App.tsx changes",
      "diff",
    );
  });
});
