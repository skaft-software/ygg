import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type {
  RuntimePolicyStatus,
  RuntimeSnapshot,
  UnavailableConsequence,
} from "../runtime-status";
import { RuntimeStatus } from "./RuntimeStatus";

function unavailablePolicy(consequence: UnavailableConsequence) {
  return {
    status: "unavailable" as const,
    reason: "This host does not publish an enforcement attestation.",
    consequence,
  };
}

function baseSnapshot(): RuntimeSnapshot {
  return {
    childAgents: [],
    mcpServers: [],
    catalog: {
      generation: 0,
      updatedAtMs: 0,
      reload: { state: "idle" },
      entries: [],
    },
    lspServers: [],
    context: {
      current: { categories: [], totalTokens: 0 },
      updatedAtMs: 0,
    },
  };
}

describe("RuntimeStatus", () => {
  afterEach(cleanup);

  it("renders truthful empty producer states without management controls", () => {
    render(<RuntimeStatus snapshot={baseSnapshot()} />);

    expect(
      screen.getByText("No child-agent observations are available"),
    ).toBeVisible();
    expect(
      screen.getByText("No MCP server observations are available"),
    ).toBeVisible();
    expect(
      screen.getByText("No language-server observations are available"),
    ).toBeVisible();
    expect(
      screen.getByText("No authoritative policy observation is available"),
    ).toBeVisible();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("states unavailable policy consequences without implying enforcement", () => {
    const policy: RuntimePolicyStatus = {
      revision: 4,
      observedAtMs: Date.UTC(2026, 6, 27, 8),
      filesystem: unavailablePolicy("hostBehaviorUnknown"),
      tools: unavailablePolicy("featureBlocked"),
      commands: unavailablePolicy("hostBehaviorUnknown"),
      remoteRead: unavailablePolicy("featureBlocked"),
      processNetwork: unavailablePolicy("hostBehaviorUnknown"),
      approvals: unavailablePolicy("hostBehaviorUnknown"),
      secrets: unavailablePolicy("featureBlocked"),
    };
    const snapshot = { ...baseSnapshot(), policy };
    render(<RuntimeStatus snapshot={snapshot} />);

    const filesystem = screen
      .getByText("Filesystem")
      .closest(".runtime-policy-card");
    expect(filesystem).not.toBeNull();
    expect(within(filesystem as HTMLElement).getByText(/Host behavior is unknown/))
      .toBeVisible();
    expect(
      within(filesystem as HTMLElement).getByText(
        "This host does not publish an enforcement attestation.",
      ),
    ).toBeVisible();

    const tools = screen.getByText("Tools").closest(".runtime-policy-card");
    expect(tools).not.toBeNull();
    expect(
      within(tools as HTMLElement).getByText(
        "Feature is blocked while enforcement is unavailable.",
      ),
    ).toBeVisible();
  });

  it("renders committed catalog, reconciled context, and compaction facts", () => {
    const snapshot: RuntimeSnapshot = {
      ...baseSnapshot(),
      catalog: {
        generation: 3,
        updatedAtMs: 300,
        reload: {
          state: "succeeded",
          reloadId: "reload-3",
          generation: 3,
          startedAtMs: 200,
          finishedAtMs: 300,
        },
        entries: [
          {
            id: "skill.review",
            label: "Review workflow",
            kind: "skill",
            enabled: true,
            contributions: [
              {
                kind: "command",
                id: "review.start",
                label: "Start review",
              },
            ],
          },
        ],
      },
      context: {
        current: {
          categories: [
            { category: "conversation", tokens: 90 },
            { category: "toolResults", tokens: 30 },
          ],
          totalTokens: 120,
        },
        updatedAtMs: 400,
        activeCompaction: {
          id: "compact-active",
          before: {
            categories: [
              { category: "conversation", tokens: 90 },
              { category: "toolResults", tokens: 30 },
            ],
            totalTokens: 120,
          },
          startedAtMs: 450,
        },
        lastCompaction: {
          id: "compact-previous",
          before: {
            categories: [{ category: "conversation", tokens: 200 }],
            totalTokens: 200,
          },
          after: {
            categories: [{ category: "conversation", tokens: 120 }],
            totalTokens: 120,
          },
          reclaimedTokens: 80,
          startedAtMs: 250,
          finishedAtMs: 300,
        },
      },
    };
    render(<RuntimeStatus snapshot={snapshot} />);

    expect(screen.getByText("Review workflow")).toBeVisible();
    expect(screen.getByText("Start review")).toBeVisible();
    expect(screen.getByText("120 tokens")).toBeVisible();
    expect(screen.getByText("Compaction in progress")).toBeVisible();
    expect(screen.getByText(/Reclaimed 80 tokens/)).toBeVisible();
    expect(screen.getByText(/committed generation 3/)).toBeVisible();
  });
});
