import { describe, expect, it } from "vitest";
import { projectRuntimeSnapshot } from "./runtime-status";

function emptySnapshot(): Record<string, unknown> {
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

describe("projectRuntimeSnapshot", () => {
  it("strictly denies unknown fields at every decoded object boundary", () => {
    expect(() =>
      projectRuntimeSnapshot({ ...emptySnapshot(), privatePath: "/tmp/ygg" }),
    ).toThrow(/runtimeSnapshot\.privatePath: unknown field/);

    const nested = emptySnapshot();
    nested.context = {
      current: { categories: [], totalTokens: 0, estimate: true },
      updatedAtMs: 0,
    };
    expect(() => projectRuntimeSnapshot(nested)).toThrow(
      /runtimeSnapshot\.context\.current\.estimate: unknown field/,
    );
  });

  it("rejects path-bearing public text and non-opaque identities", () => {
    const pathText = emptySnapshot();
    pathText.childAgents = [
      {
        id: "agent-1",
        objective: "Inspect /Users/example/private",
        state: "queued",
        queuedAtMs: 1,
        updatedAtMs: 1,
      },
    ];
    expect(() => projectRuntimeSnapshot(pathText)).toThrow(
      /contains an absolute host path/,
    );

    const pathIdentity = emptySnapshot();
    pathIdentity.mcpServers = [
      {
        id: "../server",
        label: "Server",
        state: "configured",
        restartCount: 0,
        configuredAtMs: 1,
        updatedAtMs: 1,
      },
    ];
    expect(() => projectRuntimeSnapshot(pathIdentity)).toThrow(
      /opaque runtime id/,
    );
  });

  it("accepts reconciled context totals and rejects mismatches or duplicates", () => {
    const valid = emptySnapshot();
    valid.context = {
      current: {
        categories: [
          { category: "conversation", tokens: 120 },
          { category: "toolResults", tokens: 30 },
        ],
        totalTokens: 150,
      },
      updatedAtMs: 10,
    };
    expect(projectRuntimeSnapshot(valid).context.current.totalTokens).toBe(150);

    const mismatch = structuredClone(valid);
    (
      mismatch.context as {
        current: { totalTokens: number };
      }
    ).current.totalTokens = 151;
    expect(() => projectRuntimeSnapshot(mismatch)).toThrow(
      /do not reconcile/,
    );

    const duplicate = structuredClone(valid);
    (
      duplicate.context as {
        current: {
          categories: Array<{ category: string; tokens: number }>;
          totalTokens: number;
        };
      }
    ).current = {
      categories: [
        { category: "conversation", tokens: 100 },
        { category: "conversation", tokens: 50 },
      ],
      totalTokens: 150,
    };
    expect(() => projectRuntimeSnapshot(duplicate)).toThrow(
      /contains duplicate values/,
    );
  });

  it("projects strict tagged catalog and policy variants", () => {
    const snapshot = emptySnapshot();
    snapshot.catalog = {
      generation: 2,
      updatedAtMs: 20,
      reload: {
        state: "succeeded",
        reloadId: "reload-2",
        generation: 2,
        startedAtMs: 10,
        finishedAtMs: 20,
      },
      entries: [],
    };
    snapshot.policy = {
      revision: 1,
      observedAtMs: 30,
      filesystem: {
        status: "enforced",
        access: "trustedProjectRead",
      },
      tools: {
        status: "enforced",
        rules: { default: "deny", allow: ["tool.read"], deny: [] },
      },
      commands: {
        status: "enforced",
        rules: { default: "deny", allow: ["cargo"], deny: [] },
      },
      remoteRead: {
        status: "enforced",
        consequence: {
          mode: "domainRules",
          domains: {
            default: "deny",
            allow: ["docs.example.com"],
            deny: [],
          },
        },
      },
      processNetwork: {
        status: "enforced",
        consequence: { mode: "blocked" },
      },
      approvals: {
        status: "enforced",
        consequence: {
          mode: "requiredFor",
          operations: ["filesystemWrite"],
        },
      },
      secrets: {
        status: "enforced",
        consequence: { mode: "namedGrants", grants: ["grant.docs"] },
      },
    };

    const projected = projectRuntimeSnapshot(snapshot);
    expect(projected.catalog.reload.state).toBe("succeeded");
    expect(projected.policy?.commands.status).toBe("enforced");

    const unknownNested = structuredClone(snapshot);
    (
      (
        unknownNested.policy as {
          filesystem: Record<string, unknown>;
        }
      ).filesystem
    ).hostPath = "/private";
    expect(() => projectRuntimeSnapshot(unknownNested)).toThrow(
      /runtimeSnapshot\.policy\.filesystem\.hostPath: unknown field/,
    );
  });
});
