import { describe, expect, it } from "vitest";
import eventEnvelopeGolden from "../../../extensions/ygg-serve/fixtures/event-envelope.json";
import hostBootstrapGolden from "../../../extensions/ygg-serve/fixtures/host-bootstrap.json";
import hostCommandAckGolden from "../../../extensions/ygg-serve/fixtures/host-command-ack.json";
import hostCommandGolden from "../../../extensions/ygg-serve/fixtures/host-command.json";
import liveUserDeliveryGolden from "../../../extensions/ygg-serve/fixtures/live-user-delivery.json";
import completionReviewItemGolden from "../../../extensions/ygg-serve/fixtures/completion-review-item.json";
import semanticToolEventGolden from "../../../extensions/ygg-serve/fixtures/semantic-tool-event.json";
import sessionCommandGolden from "../../../extensions/ygg-serve/fixtures/session-command.json";
import sessionSnapshotGolden from "../../../extensions/ygg-serve/fixtures/session-snapshot.json";
import {
  decodeWireCommandAck,
  encodeClientCommand,
  projectCommandDiscovery,
  projectEventEnvelope,
  projectHostBootstrap,
  projectProjectFileRead,
  projectProjectFileSearchResult,
  projectProjectFileTree,
  projectProjectFileWrite,
  projectHostStreamEvent,
  projectLifetimeUsage,
  projectProjectCatalog,
  projectRepositoryContext,
  projectSessionSnapshot,
  projectUsageActivity,
  projectUsageStats,
  WireContractError,
} from "./wire";

const clone = <T,>(value: T): T => structuredClone(value);

const repositoryContextWire = () => ({
  projectId: "prj_safe",
  trust: "verified",
  repository: {
    source: "gitStatusPorcelainV2",
    refresh: {
      state: "current",
      refreshedAtUnixMs: 1_753_626_615_000,
      durationMs: 17,
      truncated: false,
    },
    worktree: "present",
    head: "0123456789abcdef0123456789abcdef01234567",
    branchState: "named",
    branch: "feature/context",
    dirty: true,
    ahead: 2,
    behind: 1,
  },
  instructions: {
    source: "projectAgentsMdV1",
    refresh: {
      state: "current",
      refreshedAtUnixMs: 1_753_626_615_001,
      durationMs: 4,
      truncated: false,
    },
    files: [
      {
        origin: {
          relativePath: "apps/web/AGENTS.md",
          scope: "apps/web",
        },
        precedence: 1,
        byteLen: 46,
        sha256: "a".repeat(64),
        summary: "# Web instructions",
        visibleContent: "# Web instructions\nKeep changes focused.",
        contentTruncated: false,
      },
    ],
    errors: [],
    omittedErrors: 0,
    loadedBytes: 46,
  },
});

describe("authoritative Rust wire contract", () => {
  it("projects the complete host bootstrap and embedded selected session", () => {
    const { bootstrap, selectedSession } =
      projectHostBootstrap(hostBootstrapGolden);
    if (!selectedSession) {
      throw new Error("golden bootstrap must select a session");
    }

    expect(bootstrap.host).toEqual({
      id: "host-demo",
      name: "Achu's Mac",
      connection: "local",
    });
    expect(bootstrap.models[0]).toMatchObject({
      id: "gpt-5.6",
      available: true,
      reasoning: ["low", "medium", "high"],
      defaultReasoning: "high",
      inputPricing: {
        baseMicrodollarsPerMillionTokens: 2_500_000,
        tiers: [
          {
            minInputTokens: 200_000,
            microdollarsPerMillionTokens: 5_000_000,
          },
        ],
      },
      inputModalities: ["text", "image"],
    });
    expect(bootstrap.authorityProfiles).toEqual([
      "readOnly",
      "workspace",
      "fullAccess",
    ]);
    expect(bootstrap.themes[0]?.theme.typography).toEqual({
      body_family: "system-sans",
      mono_family: "system-mono",
      body_size: 17,
      display_ratio_milli: 1235,
    });
    expect(selectedSession).toMatchObject({
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 42,
      modelId: "gpt-5.6",
      reasoning: "high",
      authority: "fullAccess",
      status: "idle",
    });
    expect(selectedSession.items[0]).toMatchObject({
      kind: "assistant_message",
      content: "Ready.",
    });
    expect(bootstrap.sessions[0]?.pullRequest).toEqual({ state: "ready" });
    expect(bootstrap.capabilities.sessionBranches).toBe(true);
    expect(bootstrap.capabilities.sessionExport).toBe(true);
    expect(selectedSession.branches).toEqual({
      head: "entry-42",
      entries: [
        {
          entryId: "entry-42",
          kind: "assistantMessage",
          checkoutable: true,
          label: "Ready.",
        },
      ],
      truncated: false,
    });
  });

  it("projects inventory-only bootstraps without a selected session", () => {
    const inventory = {
      ...hostBootstrapGolden,
      selectedSessionId: null,
      selectedSession: null,
    };

    const { bootstrap, selectedSession } = projectHostBootstrap(inventory);
    expect(bootstrap.selectedSessionId).toBeNull();
    expect(selectedSession).toBeNull();
    expect(bootstrap.sessions).toHaveLength(hostBootstrapGolden.sessions.length);

    expect(() =>
      projectHostBootstrap({
        ...inventory,
        selectedSessionId: hostBootstrapGolden.selectedSessionId,
      }),
    ).toThrow(WireContractError);
  });

  it("projects root-confined project filesystem DTOs", () => {
    const tree = {
      path: "src",
      entries: [
        {
          name: "lib.rs",
          kind: "file",
          size: 42,
          modifiedAtMs: 1_753_626_615_000,
          gitStatus: [{ kind: "renamed", oldPath: "src/old.rs" }],
        },
        { name: "nested", kind: "directory", size: 0 },
      ],
      truncated: false,
      gitStatusTruncated: false,
    };
    const read = {
      path: "src/lib.rs",
      content: "export const ready = true;\n",
      startLine: 1,
      endLine: 1,
      lineCount: 1,
      truncated: false,
      sha256: "a".repeat(64),
    };
    const search = {
      hits: [{ path: "src/lib.rs", line: 1, snippet: "ready = true" }],
      truncated: false,
      scannedBytes: 42,
    };
    const write = {
      path: "src/lib.rs",
      sha256: "b".repeat(64),
      modifiedAtMs: 1_753_626_615_001,
    };

    expect(projectProjectFileTree(tree)).toEqual(tree);
    expect(projectProjectFileRead(read)).toEqual(read);
    expect(projectProjectFileSearchResult(search)).toEqual(search);
    expect(projectProjectFileWrite(write)).toEqual(write);
    expect(() =>
      projectProjectFileTree({ ...tree, path: "../outside" }),
    ).toThrow(WireContractError);
    expect(() =>
      projectProjectFileTree({
        ...tree,
        entries: [{ ...tree.entries[0], gitStatus: [{ kind: "unknown" }] }],
      }),
    ).toThrow(WireContractError);
    expect(() =>
      projectProjectFileRead({ ...read, sha256: "A".repeat(64) }),
    ).toThrow(WireContractError);

    const bootstrap = clone(hostBootstrapGolden) as {
      capabilities: Record<string, unknown>;
    };
    bootstrap.capabilities.projectFileBrowser = true;
    bootstrap.capabilities.projectFileWrite = true;
    expect(projectHostBootstrap(bootstrap).bootstrap.capabilities).toMatchObject({
      projectFileBrowser: true,
      projectFileWrite: true,
    });
    bootstrap.capabilities.projectFileBrowser = false;
    expect(() => projectHostBootstrap(bootstrap)).toThrow(WireContractError);
  });

  it("projects the standalone session snapshot against the model catalog", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const summary = bootstrap.sessions[0];
    const snapshot = projectSessionSnapshot(sessionSnapshotGolden, {
      summary,
      models: bootstrap.models,
      timestampMs: 1_721_000_000_042,
    });

    expect(snapshot.sequence).toBe(42);
    expect(snapshot.contextTokens).toBe(165);
    expect(snapshot.context.status.current).toEqual({
      categories: [{ category: "other", tokens: 165 }],
      totalTokens: 165,
    });
    expect(snapshot.contextPercent).toBe(0);
    expect(snapshot.title).toBe("New session");
    expect(snapshot.items).toHaveLength(1);
  });

  it("strictly projects replayable context lifecycle and compaction updates", () => {
    const context = {
      usage: {
        inputTokens: 300,
        outputTokens: 20,
        contextTokens: 120,
        contextLimit: 1_000,
      },
      compactions: 1,
      status: {
        current: {
          categories: [
            { category: "conversation", tokens: 80 },
            { category: "documents", tokens: 20 },
            { category: "projectFiles", tokens: 10 },
            { category: "other", tokens: 10 },
          ],
          totalTokens: 120,
        },
        updatedAtMs: 500,
        lastCompaction: {
          id: "run-1:compaction:1",
          reason: "threshold",
          before: {
            categories: [{ category: "conversation", tokens: 200 }],
            totalTokens: 200,
          },
          after: {
            categories: [
              { category: "conversation", tokens: 80 },
              { category: "documents", tokens: 20 },
              { category: "projectFiles", tokens: 10 },
              { category: "other", tokens: 10 },
            ],
            totalTokens: 120,
          },
          reclaimedTokens: 80,
          succeeded: true,
          startedAtMs: 450,
          finishedAtMs: 500,
        },
      },
      run: {
        phase: "retrying",
        responsesStarted: 2,
        responsesFinished: 1,
        responsesDiscarded: 1,
        responseActive: false,
        toolCallsStarted: 1,
        toolCallsFinished: 1,
        toolExecutionsStarted: 1,
        toolExecutionsFinished: 1,
        compactionsStarted: 1,
        compactionsCompleted: 1,
        compactionsFailed: 0,
      },
    };
    const envelope = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    envelope.cursor.sequence = 44;
    envelope.event = {
      type: "context.updated",
      data: { context },
    };

    const projected = projectEventEnvelope(envelope);
    expect(projected).toMatchObject({
      type: "context.updated",
      sequence: 44,
      context: {
        compactions: 1,
        run: {
          phase: "retrying",
          responsesStarted: 2,
          responsesDiscarded: 1,
        },
        status: {
          current: { totalTokens: 120 },
          lastCompaction: {
            reason: "threshold",
            reclaimedTokens: 80,
            succeeded: true,
          },
        },
      },
    });

    const unknown = clone(envelope) as typeof envelope & {
      event: { data: { context: { status: Record<string, unknown> } } };
    };
    unknown.event.data.context.status.providerAttribution = true;
    expect(() => projectEventEnvelope(unknown)).toThrow(
      /context\.status\.providerAttribution is not supported/,
    );

    const contradictory = clone(envelope) as typeof envelope & {
      event: {
        data: {
          context: {
            run: {
              phase: string;
              responsesStarted: number;
              responseActive: boolean;
            };
          };
        };
      };
    };
    contradictory.event.data.context.run.responsesStarted = 3;
    expect(() => projectEventEnvelope(contradictory)).toThrow(
      /contradictory lifecycle counters/,
    );

    contradictory.event.data.context.run.responsesStarted = 2;
    contradictory.event.data.context.run.phase = "responding";
    contradictory.event.data.context.run.responseActive = false;
    expect(() => projectEventEnvelope(contradictory)).toThrow(
      /contradictory lifecycle counters/,
    );
  });

  it("accepts unchanged failed compactions and rejects fabricated reclamation", () => {
    const totals = {
      categories: [{ category: "conversation", tokens: 120 }],
      totalTokens: 120,
    };
    const context = {
      usage: {
        inputTokens: 120,
        outputTokens: 0,
        contextTokens: 120,
      },
      compactions: 0,
      status: {
        current: totals,
        updatedAtMs: 600,
        lastCompaction: {
          id: "run-2:compaction:1",
          reason: "overflow",
          before: totals,
          after: totals,
          reclaimedTokens: 0,
          succeeded: false,
          startedAtMs: 550,
          finishedAtMs: 600,
        },
      },
      run: {
        phase: "preparing",
        responsesStarted: 1,
        responsesFinished: 0,
        responsesDiscarded: 1,
        responseActive: false,
        toolCallsStarted: 0,
        toolCallsFinished: 0,
        toolExecutionsStarted: 0,
        toolExecutionsFinished: 0,
        compactionsStarted: 1,
        compactionsCompleted: 0,
        compactionsFailed: 1,
      },
    };
    const envelope = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    envelope.cursor.sequence = 44;
    envelope.event = { type: "context.updated", data: { context } };

    expect(projectEventEnvelope(envelope)).toMatchObject({
      type: "context.updated",
      context: {
        status: {
          lastCompaction: {
            reason: "overflow",
            reclaimedTokens: 0,
            succeeded: false,
          },
        },
        run: { compactionsFailed: 1 },
      },
    });

    const malformed = clone(envelope) as typeof envelope & {
      event: {
        data: {
          context: {
            status: {
              lastCompaction: {
                after: { categories: unknown[]; totalTokens: number };
                reclaimedTokens: number;
              };
            };
          };
        };
      };
    };
    malformed.event.data.context.status.lastCompaction.after = {
      categories: [{ category: "conversation", tokens: 100 }],
      totalTokens: 100,
    };
    malformed.event.data.context.status.lastCompaction.reclaimedTokens = 20;
    expect(() => projectEventEnvelope(malformed)).toThrow(
      /contradictory completed-compaction facts/,
    );
  });

  it("rehydrates durable evidence origins and exact file handles from a snapshot", () => {
    const durable = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
      sources?: unknown[];
      artifacts?: unknown[];
    };
    durable.items.push(
      {
        id: "item-tool-read",
        turnId: "turn-evidence",
        lifecycle: "committed",
        durableEntryId: "entry-tool",
        payload: {
          type: "toolCall",
          data: {
            rawToolName: "read",
            kind: "read",
            phase: "investigated",
            status: "succeeded",
            title: "Read source file",
            summary: "Read src/theme.ts",
            target: "src/theme.ts",
            startedAtMs: 1_721_000_000_040,
            completedAtMs: 1_721_000_000_041,
            durationMs: 1,
            observedOutputBytes: 128,
            droppedOutputBytes: 0,
          },
        },
      },
      {
        id: "item-file-change",
        turnId: "turn-evidence",
        lifecycle: "committed",
        durableEntryId: "entry-result",
        payload: {
          type: "fileChange",
          data: {
            handle: "resource-diff",
            resultHandle: "resource-result",
            displayPath: "src/theme.ts",
            additions: 8,
            deletions: 3,
          },
        },
      },
    );
    durable.sources = [
      {
        id: "source-theme",
        kind: "file",
        title: "src/theme.ts",
        handle: "resource-source",
        originItemId: "item-tool-read",
        consultedAtMs: 1_721_000_000_044,
        cited: false,
        available: true,
      },
    ];
    durable.artifacts = [
      {
        id: "artifact-theme",
        kind: "file",
        name: "theme.ts",
        mediaType: "text/plain",
        handle: "resource-result",
        byteLen: 128,
        originItemId: "item-tool-read",
        available: true,
      },
    ];
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = projectSessionSnapshot(durable, {
      summary: bootstrap.sessions[0],
      models: bootstrap.models,
    });

    expect(snapshot.sources[0]).toMatchObject({
      id: "source-theme",
      handle: "resource-source",
      originItemId: "item-tool-read",
    });
    expect(snapshot.outputs[0]).toMatchObject({
      id: "artifact-theme",
      handle: "resource-result",
      originItemId: "item-tool-read",
    });
    expect(
      snapshot.items.find((item) => item.id === "item-file-change"),
    ).toMatchObject({
      kind: "action",
      diffHandle: "resource-diff",
      resultHandle: "resource-result",
    });
  });

  it("accepts omitted branch parents only when truncation is explicit", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const summary = bootstrap.sessions[0];
    const truncated = {
      ...clone(sessionSnapshotGolden),
      durableHead: "entry-recent",
      branches: {
        head: "entry-recent",
        entries: [
          {
            entryId: "entry-recent",
            parentEntryId: "entry-omitted",
            kind: "assistantMessage",
            checkoutable: true,
            label: "Recent answer",
          },
        ],
        truncated: true,
      },
    };
    expect(
      projectSessionSnapshot(truncated, {
        summary,
        models: bootstrap.models,
      }).branches.truncated,
    ).toBe(true);

    const complete = {
      ...truncated,
      branches: { ...truncated.branches, truncated: false },
    };
    expect(() =>
      projectSessionSnapshot(complete, {
        summary,
        models: bootstrap.models,
      }),
    ).toThrow(/parent outside the preserved graph/);
  });

  it("projects the sequenced event and nested host stream envelope", () => {
    const event = projectEventEnvelope(eventEnvelopeGolden);
    expect(event).toEqual({
      type: "item.delta",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 43,
      itemId: "item-stream",
      field: "content",
      delta: " world",
    });

    expect(
      projectHostStreamEvent({
        protocol: 1,
        hostSequence: 12,
        event: eventEnvelopeGolden,
      }),
    ).toEqual({ hostSequence: 12, event });
  });

  it("projects the exact semantic activity and completion-review goldens", () => {
    expect(projectEventEnvelope(semanticToolEventGolden)).toMatchObject({
      type: "item.activity",
      sessionId: "session-demo",
      sequence: 44,
      itemId: "item-tool-cargo-test",
      activity: {
        rawToolName: "bash",
        actionKind: "command",
        phase: "verified",
        status: "succeeded",
        label: "Run cargo test",
        commandPreview: "cargo test",
        exitCode: 0,
        durationMs: 250,
        outputSummary: "Verification completed",
        observedOutputBytes: 4096,
        droppedOutputBytes: 128,
      },
    });

    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
    };
    snapshot.items.push(completionReviewItemGolden);
    expect(
      projectSessionSnapshot(snapshot, {
        summary: bootstrap.sessions[0],
        models: bootstrap.models,
      }).items.at(-1),
    ).toMatchObject({
      id: "item-run-outcome",
      runId: "run-stable-1",
      kind: "run_outcome",
      outcome: "done",
      review: {
        actionCount: 2,
        changedFileItemIds: ["item-file-change"],
        verificationActionItemIds: ["item-tool-cargo-test"],
        outputIds: ["artifact-report"],
        evidenceCoverage: "partial",
      },
    });
  });

  it("uses the sanitized terminal diagnostic as the failed run summary", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
    };
    const failed = clone(completionReviewItemGolden) as unknown as {
      payload: {
        data: {
          outcome: string;
          message?: string;
        };
      };
    };
    failed.payload.data.outcome = "failed";
    failed.payload.data.message =
      "provider=custom/e2e model=e2e-model phase=connection";
    snapshot.items.push(failed);

    expect(
      projectSessionSnapshot(snapshot, {
        summary: bootstrap.sessions[0],
        models: bootstrap.models,
      }).items.at(-1),
    ).toMatchObject({
      kind: "run_outcome",
      outcome: "failed",
      summary: "provider=custom/e2e model=e2e-model phase=connection",
    });
  });

  it("strictly projects structured test evidence in a completion review", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
    };
    snapshot.items.push({
      id: "item-run-with-tests",
      runId: "run-tests",
      turnId: "turn-tests",
      lifecycle: "committed",
      durableEntryId: "entry-run-with-tests",
      payload: {
        type: "runOutcome",
        data: {
          outcome: "completed",
          review: {
            summary: "Supported reporter evidence was parsed.",
            durationMs: 980,
            actionCount: 1,
            phases: [],
            changedFileItemIds: [],
            verificationActionItemIds: ["item-vitest"],
            failedActionItemIds: [],
            warningActionItemIds: [],
            sourceIds: [],
            outputIds: [],
            evidenceCoverage: "complete",
            openQuestions: [],
            testResults: [
              {
                originItemId: "item-vitest",
                framework: "vitest",
                parser: "vitestTextV1",
                command: { status: "succeeded", exitCode: 0 },
                verification: "passed",
                reported: {
                  total: 3,
                  passed: 3,
                  failed: 0,
                  skipped: 0,
                },
                reportedSuites: {
                  total: 1,
                  passed: 1,
                  failed: 0,
                },
                summaryCount: 1,
                suites: [
                  {
                    name: "src/wire.test.ts",
                    status: "passed",
                    reported: {
                      total: 3,
                      passed: 3,
                      failed: 0,
                    },
                    cases: [
                      { name: "rejects unknown keys", status: "passed" },
                    ],
                  },
                ],
                coverage: {
                  inputTruncated: false,
                  recordsTruncated: false,
                  unsupportedSummaryFields: false,
                  summaries: "complete",
                  cases: "partial",
                },
              },
            ],
          },
        },
      },
    });

    const outcome = projectSessionSnapshot(snapshot, {
      summary: bootstrap.sessions[0],
      models: bootstrap.models,
    }).items.at(-1);
    expect(outcome).toMatchObject({
      kind: "run_outcome",
      review: {
        testResults: [
          {
            originItemId: "item-vitest",
            framework: "vitest",
            verification: "passed",
            reported: { total: 3, passed: 3, failed: 0, skipped: 0 },
            suites: [
              {
                name: "src/wire.test.ts",
                status: "passed",
                cases: [
                  {
                    name: "rejects unknown keys",
                    status: "passed",
                  },
                ],
              },
            ],
            coverage: {
              summaries: "complete",
              cases: "partial",
            },
          },
        ],
      },
    });
  });

  it("rejects unknown or malformed nested structured test evidence", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
    };
    const result = {
      originItemId: "item-vitest",
      framework: "vitest",
      parser: "vitestTextV1",
      command: { status: "succeeded", exitCode: 0 },
      verification: "passed",
      reported: { total: 1, passed: 1 },
      reportedSuites: { total: 1, passed: 1 },
      summaryCount: 1,
      suites: [
        {
          name: "src/wire.test.ts",
          reported: { total: 1, passed: 1 },
          cases: [{ name: "safe case", status: "passed" }],
        },
      ],
      coverage: {
        inputTruncated: false,
        recordsTruncated: false,
        unsupportedSummaryFields: false,
        summaries: "complete",
        cases: "complete",
      },
    };
    const item = (testResult: unknown) => ({
      id: "item-run-with-tests",
      turnId: "turn-tests",
      lifecycle: "committed",
      payload: {
        type: "runOutcome",
        data: {
          outcome: "completed",
          review: {
            summary: "Test evidence",
            durationMs: 1,
            actionCount: 1,
            evidenceCoverage: "complete",
            testResults: [testResult],
          },
        },
      },
    });

    snapshot.items.push(
      item({
        ...result,
        suites: [{ ...result.suites[0], hostPath: "/private/repo" }],
      }),
    );
    expect(() =>
      projectSessionSnapshot(snapshot, {
        summary: bootstrap.sessions[0],
        models: bootstrap.models,
      }),
    ).toThrowError(
      expect.objectContaining({
        name: "WireContractError",
        path: expect.stringContaining("testResults[0].suites[0].hostPath"),
      }),
    );

    snapshot.items.splice(
      -1,
      1,
      item({ ...result, reported: { total: -1 } }),
    );
    expect(() =>
      projectSessionSnapshot(snapshot, {
        summary: bootstrap.sessions[0],
        models: bootstrap.models,
      }),
    ).toThrow(/testResults\[0\]\.reported\.total/);
  });

  it("strictly projects path-free repository context", () => {
    expect(projectRepositoryContext(repositoryContextWire())).toEqual(
      repositoryContextWire(),
    );

    const unknown = repositoryContextWire() as ReturnType<
      typeof repositoryContextWire
    > & {
      repository: ReturnType<
        typeof repositoryContextWire
      >["repository"] & { rootPath: string };
    };
    unknown.repository.rootPath = "/Users/example/project";
    expect(() => projectRepositoryContext(unknown)).toThrowError(
      expect.objectContaining({
        name: "WireContractError",
        path: "repositoryContext.repository.rootPath",
      }),
    );
  });

  it.each([
    "/Users/example/project/AGENTS.md",
    "../AGENTS.md",
    "apps\\web\\AGENTS.md",
    "C:/workspace/AGENTS.md",
  ])("rejects path-bearing instruction origin %s", (relativePath) => {
    const context = repositoryContextWire();
    context.instructions.files[0]!.origin.relativePath = relativePath;
    expect(() => projectRepositoryContext(context)).toThrowError(
      expect.objectContaining({
        name: "WireContractError",
        path:
          "repositoryContext.instructions.files[0].origin.relativePath",
      }),
    );
  });

  it("preserves live user-message delivery semantics", () => {
    const queued = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    queued.cursor.sequence = 44;
    queued.event = {
      type: "item.started",
      data: {
        item: liveUserDeliveryGolden,
      },
    };

    expect(projectEventEnvelope(queued)).toMatchObject({
      type: "item.started",
      item: {
        kind: "user_message",
        content: "Change direction",
        state: "streaming",
        delivery: "steer",
      },
    });
  });

  it("projects complete cross-client catalog summary changes", () => {
    const summary = clone(hostBootstrapGolden.sessions[0]);
    summary.id = "session-from-other-client";
    summary.title = "Created on Achu’s phone";
    summary.attention = "unreadCompletion";

    expect(
      projectHostStreamEvent(
        {
          protocol: 1,
          hostSequence: 13,
          catalog: {
            catalogCursor: 9,
            summary,
          },
        },
        { models: projectHostBootstrap(hostBootstrapGolden).bootstrap.models },
      ),
    ).toMatchObject({
      hostSequence: 13,
      event: {
        type: "catalog.summary",
        catalogRevision: 9,
        summary: {
          id: "session-from-other-client",
          title: "Created on Achu’s phone",
          unread: true,
          attentionCount: 0,
        },
      },
    });
  });

  it("preserves opaque resource handles for safe source and artifact viewers", () => {
    const source = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    source.cursor.sequence = 44;
    source.event = {
      type: "source.upserted",
      data: {
        source: {
          id: "source-theme",
          kind: "file",
          title: "theme.ts",
          handle: "resource-source-theme",
          originItemId: "item-tool-theme",
          consultedAtMs: 1_721_000_000_044,
          cited: false,
          available: true,
        },
      },
    };
    expect(projectEventEnvelope(source)).toMatchObject({
      type: "session.resources",
      sources: [
        {
          id: "source-theme",
          handle: "resource-source-theme",
          originItemId: "item-tool-theme",
          available: true,
        },
      ],
    });

    const artifact = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    artifact.cursor.sequence = 45;
    artifact.event = {
      type: "artifact.upserted",
      data: {
        artifact: {
          id: "artifact-report",
          kind: "document",
          name: "report.md",
          mediaType: "text/markdown",
          handle: "resource-artifact-report",
          originItemId: "item-tool-report",
          byteLen: 128,
          available: true,
        },
      },
    };
    expect(projectEventEnvelope(artifact)).toMatchObject({
      type: "session.resources",
      outputs: [
        {
          id: "artifact-report",
          handle: "resource-artifact-report",
          originItemId: "item-tool-report",
          mimeType: "text/markdown",
          available: true,
        },
      ],
    });
  });

  it("preserves exact diff and resulting-file handles", () => {
    const changed = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    changed.cursor.sequence = 46;
    changed.event = {
      type: "item.committed",
      data: {
        item: {
          id: "item-file-change",
          turnId: "turn-change",
          lifecycle: "committed",
          durableEntryId: "entry-change",
          payload: {
            type: "fileChange",
            data: {
              handle: "resource-exact-diff",
              resultHandle: "resource-exact-result",
              displayPath: "src/theme.ts",
              additions: 8,
              deletions: 3,
            },
          },
        },
      },
    };

    expect(projectEventEnvelope(changed)).toMatchObject({
      type: "item.committed",
      item: {
        kind: "action",
        actionKind: "file_write",
        target: "src/theme.ts",
        additions: 8,
        deletions: 3,
        diffHandle: "resource-exact-diff",
        resultHandle: "resource-exact-result",
      },
    });
  });

  it("projects the exact durable branch head", () => {
    const durable = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    durable.cursor.sequence = 44;
    durable.event = {
      type: "session.durableHeadChanged",
      data: { durableEntryId: "entry-44" },
    };

    expect(projectEventEnvelope(durable)).toEqual({
      type: "session.durableHeadChanged",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      durableHead: "entry-44",
    });
  });

  it("projects pull-request evidence changes and explicit removal", () => {
    const changed = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    changed.cursor.sequence = 44;
    changed.event = {
      type: "session.pullRequestChanged",
      data: { pullRequest: { state: "inProgress" } },
    };

    expect(projectEventEnvelope(changed)).toEqual({
      type: "session.pullRequestChanged",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      pullRequest: { state: "in_progress" },
    });

    changed.cursor.sequence = 45;
    changed.event = {
      type: "session.pullRequestChanged",
      data: { pullRequest: null },
    };
    expect(projectEventEnvelope(changed)).toEqual({
      type: "session.pullRequestChanged",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 45,
      pullRequest: null,
    });
  });

  it("preserves active-run identity while a session needs attention", () => {
    const waiting = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    waiting.cursor.sequence = 44;
    waiting.event = {
      type: "session.stateChanged",
      data: { state: "needsInput", activeRunId: "run-live" },
    };

    expect(projectEventEnvelope(waiting)).toEqual({
      type: "session.updated",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      patch: {
        status: "needs_attention",
        activeRunId: "run-live",
      },
    });
  });

  it("projects and encodes durable session metadata changes", () => {
    const metadata = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    metadata.cursor.sequence = 44;
    metadata.event = {
      type: "session.metadataChanged",
      data: {
        title: "Renamed session",
        pinned: true,
        archived: false,
      },
    };
    expect(projectEventEnvelope(metadata)).toEqual({
      type: "session.updated",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      patch: { title: "Renamed session" },
    });

    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-rename",
          type: "session.rename",
          sessionId: "session-demo",
          title: "Renamed session",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.rename",
        data: { title: "Renamed session" },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-pin",
          type: "session.pin",
          sessionId: "session-demo",
          pinned: true,
        },
        context,
      ),
    ).toMatchObject({
      command: { type: "session.pin", data: { pinned: true } },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-archive",
          type: "session.archive",
          sessionId: "session-demo",
          archived: true,
        },
        context,
      ),
    ).toMatchObject({
      command: { type: "session.archive", data: { archived: true } },
    });
  });

  it("projects and answers typed user-input requests", () => {
    const request = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      timestampMs: number;
      event: unknown;
    };
    request.cursor.sequence = 44;
    request.timestampMs = 1_721_000_000_044;
    request.event = {
      type: "request.changed",
      data: {
        request: {
          id: "request-input",
          actorGeneration: 3,
          kind: {
            type: "userInput",
            data: {
              prompt: "Which layout should I keep?",
              choices: ["Compact", "Comfortable"],
            },
          },
          state: "pending",
        },
      },
    };

    expect(projectEventEnvelope(request)).toMatchObject({
      type: "item.committed",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      item: {
        id: "request-request-input",
        kind: "user_input_request",
        requestId: "request-input",
        prompt: "Which layout should I keep?",
        choices: ["Compact", "Comfortable"],
        state: "streaming",
      },
    });

    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-input-choice",
          type: "userInput.resolve",
          sessionId: "session-demo",
          requestId: "request-input",
          answer: { type: "choice", choice: "Compact" },
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: {
        type: "session.answerRequest",
        data: {
          requestId: "request-input",
          answer: {
            type: "choice",
            data: { choice: "Compact" },
          },
        },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-input-text",
          type: "userInput.resolve",
          sessionId: "session-demo",
          requestId: "request-input",
          answer: { type: "text", text: "Use the denser option." },
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.answerRequest",
        data: {
          answer: {
            type: "text",
            data: { text: "Use the denser option." },
          },
        },
      },
    });
  });

  it("coalesces linked tool results into their action cell", () => {
    const toolCall = {
      id: "tool-call-1",
      turnId: "turn-tool",
      providerAttempt: 2,
      lifecycle: "committed",
      durableEntryId: "entry-tool-call",
      payload: {
        type: "toolCall",
        data: {
          rawToolName: "shell",
          kind: "command",
          phase: "verified",
          status: "succeeded",
          title: "Run tests",
          summary: "Verification completed",
          commandPreview: "npm test",
          exitCode: 0,
          startedAtMs: 1_721_000_000_100,
          completedAtMs: 1_721_000_001_100,
          durationMs: 1_000,
          outputSummary: "43 tests passed",
          observedOutputBytes: 2_048,
          droppedOutputBytes: 0,
        },
      },
    };
    const toolResult = {
      id: "tool-result-1",
      turnId: "turn-tool",
      lifecycle: "committed",
      durableEntryId: "entry-tool-result",
      payload: {
        type: "toolResult",
        data: {
          toolCallItemId: "tool-call-1",
          status: "succeeded",
          summary: "43 tests passed",
          outputSummary: "43 tests passed",
          exitCode: 0,
          completedAtMs: 1_721_000_001_100,
          durationMs: 1_000,
          observedOutputBytes: 2_048,
          droppedOutputBytes: 0,
        },
      },
    };
    const snapshot = {
      ...sessionSnapshotGolden,
      items: [toolCall, toolResult],
    };

    expect(
      projectSessionSnapshot(snapshot, {
        summary: projectHostBootstrap(hostBootstrapGolden).bootstrap.sessions[0],
        models: projectHostBootstrap(hostBootstrapGolden).bootstrap.models,
      }).items,
    ).toEqual([
      expect.objectContaining({
        id: "tool-call-1",
        kind: "action",
        providerAttempt: 2,
        durableEntryId: "entry-tool-call",
        label: "Run tests",
        detail: "43 tests passed",
        status: "succeeded",
        state: "committed",
      }),
    ]);

    const resultEvent = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    resultEvent.cursor.sequence = 44;
    resultEvent.event = {
      type: "item.committed",
      data: { item: toolResult },
    };
    expect(projectEventEnvelope(resultEvent)).toMatchObject({
      type: "item.activity_result",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      itemId: "tool-call-1",
      resultItemId: "tool-result-1",
      result: {
        status: "succeeded",
        summary: "43 tests passed",
        outputSummary: "43 tests passed",
        exitCode: 0,
        durationMs: 1_000,
        observedOutputBytes: 2_048,
        droppedOutputBytes: 0,
      },
    });
  });

  it("encodes the exact host command golden", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const encoded = encodeClientCommand(
      {
        id: "command-create",
        type: "session.create",
        projectId: "project-ygg",
        modelId: "gpt-5.6",
        reasoning: "high",
        authority: "fullAccess",
      },
      {
        hostId: "host-demo",
        deviceId: "device-browser",
        issuedAtMs: 1_721_000_000_060,
        actorGenerationBySession: {},
        modelIdBySession: {},
        models: bootstrap.models,
      },
    );

    expect(encoded).toEqual(hostCommandGolden);
  });

  it("encodes the exact generation-bound session command golden", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const encoded = encodeClientCommand(
      {
        id: "command-submit",
        type: "session.submit",
        sessionId: "session-demo",
        prompt: "Review this image",
        attachments: [
          {
            id: "image-1",
            handle: "upload:image-1",
            name: "alignment.png",
            mediaType: "image/png",
            size: 98_765,
          },
        ],
      },
      {
        hostId: "host-demo",
        deviceId: "device-browser",
        issuedAtMs: 1_721_000_000_050,
        actorGenerationBySession: { "session-demo": 3 },
        modelIdBySession: { "session-demo": "gpt-5.6" },
        models: bootstrap.models,
      },
    );

    expect(encoded).toEqual(sessionCommandGolden);
  });

  it("encodes explicit active-run steer and follow-up commands", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_052,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    const input = {
      text: "Use the smaller layout",
      attachments: [],
    };

    expect(
      encodeClientCommand(
        {
          id: "command-steer",
          type: "session.steer",
          sessionId: "session-demo",
          prompt: input.text,
          attachments: [],
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: { type: "session.steer", data: { input } },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-follow-up",
          type: "session.followUp",
          sessionId: "session-demo",
          prompt: input.text,
          attachments: [],
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: { type: "session.followUp", data: { input } },
    });
  });

  it("encodes the exact durable checkout command and replacement signal", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_053,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-checkout",
          type: "session.checkout",
          sessionId: "session-demo",
          entryId: "entry-17",
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: {
        type: "session.checkout",
        data: { entryId: "entry-17" },
      },
    });

    const replacement = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    replacement.cursor.sequence = 44;
    replacement.event = {
      type: "session.projectionReplaced",
      data: { durableEntryId: "entry-17" },
    };
    expect(projectEventEnvelope(replacement)).toEqual({
      type: "session.projectionReplaced",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      durableHead: "entry-17",
    });
  });

  it("encodes conversation branches, trash lifecycle, and durable provenance", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_054,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };

    expect(
      encodeClientCommand(
        {
          id: "command-edit",
          type: "session.editUserTurn",
          sessionId: "session-demo",
          sourceUserEntryId: "entry-user",
          prompt: "Replacement turn",
          attachments: [],
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: {
        type: "session.editUserTurn",
        data: {
          sourceUserEntryId: "entry-user",
          input: { text: "Replacement turn", attachments: [] },
        },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-retry-model",
          type: "session.retryResponse",
          sessionId: "session-demo",
          sourceAssistantEntryId: "entry-assistant",
          modelId: "gpt-5.6",
          reasoning: "medium",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.retryResponse",
        data: {
          sourceAssistantEntryId: "entry-assistant",
          model: {
            provider: "openai",
            model: "gpt-5.6",
            reasoning: "medium",
          },
        },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-fork",
          type: "session.forkConversation",
          sessionId: "session-demo",
          entryId: "entry-assistant",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.forkConversation",
        data: { entryId: "entry-assistant" },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-trash",
          type: "session.setLifecycle",
          sessionId: "session-demo",
          lifecycle: "trash",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.setLifecycle",
        data: { sessionId: "session-demo", lifecycle: "trash" },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-delete",
          type: "session.deletePermanently",
          sessionId: "session-demo",
          confirmation: {
            sessionId: "session-demo",
            trashedAtMs: 1_721_000_000_000,
            phrase: "permanently delete session-demo",
          },
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.deletePermanently",
        data: {
          sessionId: "session-demo",
          confirmation: {
            sessionId: "session-demo",
            trashedAtMs: 1_721_000_000_000,
            phrase: "permanently delete session-demo",
          },
        },
      },
    });

    const event = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    event.cursor.sequence = 46;
    event.event = {
      type: "item.committed",
      data: {
        item: {
          id: "item-edited-user",
          turnId: "turn-edited",
          lifecycle: "committed",
          durableEntryId: "entry-edited-user",
          payload: {
            type: "userMessage",
            data: {
              text: "Replacement turn",
              attachments: [],
              branchProvenance: {
                operation: "editUserTurn",
                sourceSessionId: "session-demo",
                sourceEntryId: "entry-user",
                externalEffectsPreserved: true,
                warning:
                  "External side effects from the earlier transcript are preserved.",
              },
            },
          },
        },
      },
    };
    expect(projectEventEnvelope(event)).toMatchObject({
      type: "item.committed",
      item: {
        kind: "user_message",
        branchProvenance: {
          operation: "editUserTurn",
          sourceEntryId: "entry-user",
          externalEffectsPreserved: true,
        },
      },
    });
  });

  it("decodes the exact host acknowledgement golden and session acks", () => {
    expect(decodeWireCommandAck(hostCommandAckGolden)).toEqual({
      commandId: "command-create",
      accepted: true,
      createdSessionId: "session-created",
    });
    expect(
      decodeWireCommandAck({
        protocol: 1,
        sessionId: "session-demo",
        commandId: "command-submit",
        acknowledgedAtMs: 1_721_000_000_051,
        cursor: { actorGeneration: 3, sequence: 43 },
        disposition: { status: "accepted", runId: "run-1" },
      }),
    ).toEqual({
      commandId: "command-submit",
      accepted: true,
      createdSessionId: undefined,
    });
    expect(
      decodeWireCommandAck({
        protocol: 1,
        sessionId: "session-demo",
        commandId: "command-retry",
        acknowledgedAtMs: 1_721_000_000_052,
        cursor: { actorGeneration: 4, sequence: 0 },
        disposition: {
          status: "rejected",
          error: {
            code: "staleGeneration",
            message: "The session generation changed.",
            retryable: true,
            currentGeneration: 4,
          },
        },
      }),
    ).toEqual({
      commandId: "command-retry",
      accepted: false,
      error: "The session generation changed.",
      errorCode: "staleGeneration",
      retryable: true,
      currentGeneration: 4,
    });
    expect(
      decodeWireCommandAck({
        protocol: 1,
        sessionId: "session-demo",
        commandId: "command-fork",
        acknowledgedAtMs: 1_721_000_000_053,
        cursor: { actorGeneration: 3, sequence: 44 },
        disposition: {
          status: "accepted",
          createdSessionId: "session-forked",
        },
      }),
    ).toEqual({
      commandId: "command-fork",
      accepted: true,
      createdSessionId: "session-forked",
    });
  });

  it("projects the path-free project catalog and exact lifecycle commands", () => {
    const project = {
      id: "prj_11111111111111111111111111111111",
      name: "ygg",
      trusted: false,
      archived: false,
      available: true,
      isDefault: false,
      sessionCount: 2,
      liveSessionCount: 0,
    };
    expect(
      projectProjectCatalog({
        protocol: 1,
        host: { id: "host-demo", name: "Local Mac" },
        catalogCursor: 9,
        lifecycleMutationsSupported: true,
        importSupported: false,
        projects: [project],
      }),
    ).toEqual({
      host: { id: "host-demo", name: "Local Mac" },
      catalogRevision: 9,
      lifecycleMutationsSupported: true,
      importSupported: false,
      projects: [project],
    });

    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: {},
      modelIdBySession: {},
      models: [],
    };
    expect(
      encodeClientCommand(
        {
          id: "command-trust",
          type: "project.setTrust",
          projectId: project.id,
          trusted: true,
        },
        context,
      ),
    ).toEqual({
      protocol: 1,
      hostId: "host-demo",
      deviceId: "device-browser",
      commandId: "command-trust",
      issuedAtMs: 1_721_000_000_060,
      command: {
        type: "project.setTrust",
        data: { projectId: project.id, trusted: true },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-import",
          type: "project.import",
          candidateId: "candidate-host-picker-1",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "project.import",
        data: { candidateId: "candidate-host-picker-1" },
      },
    });
    expect(
      JSON.stringify(
        encodeClientCommand(
          {
            id: "command-import",
            type: "project.import",
            candidateId: "candidate-host-picker-1",
          },
          context,
        ),
      ),
    ).not.toMatch(/root|path/i);

    expect(
      decodeWireCommandAck({
        protocol: 1,
        hostId: "host-demo",
        commandId: "command-trust",
        acknowledgedAtMs: 1_721_000_000_061,
        catalogCursor: 10,
        disposition: {
          status: "accepted",
          project: { ...project, trusted: true },
        },
      }),
    ).toEqual({
      commandId: "command-trust",
      accepted: true,
      createdSessionId: undefined,
      project: { ...project, trusted: true },
      catalogChanged: undefined,
    });
  });

  it("projects command discovery strictly and encodes typed slash invocation", () => {
    const discovery = {
      protocol: 1,
      commands: [
        {
          name: "compact",
          usage: "/compact",
          description: "Compact the conversation context.",
          acceptsArgument: false,
          kind: "builtIn",
        },
        {
          name: "review",
          usage: "/review [focus]",
          description: "Review the current implementation.",
          argumentHint: "[focus]",
          acceptsArgument: true,
          kind: "prompt",
        },
      ],
      skills: [
        {
          id: "testing",
          name: "Testing",
          description: "Run focused tests.",
          active: true,
        },
      ],
    };
    expect(projectCommandDiscovery(discovery)).toEqual({
      commands: discovery.commands,
      skills: discovery.skills,
    });
    expect(() =>
      projectCommandDiscovery({
        ...discovery,
        commands: [...discovery.commands, discovery.commands[0]],
      }),
    ).toThrow(new WireContractError("commandDiscovery.commands[2].name", "is duplicated"));
    expect(() =>
      projectCommandDiscovery({
        ...discovery,
        skills: [...discovery.skills, discovery.skills[0]],
      }),
    ).toThrow(new WireContractError("commandDiscovery.skills[1].id", "is duplicated"));

    expect(
      encodeClientCommand(
        {
          id: "command-slash",
          type: "session.invokeSlashCommand",
          sessionId: "session-demo",
          invocation: " /compact ",
        },
        {
          hostId: "host-demo",
          deviceId: "device-browser",
          issuedAtMs: 1_721_000_000_062,
          actorGenerationBySession: { "session-demo": 3 },
          modelIdBySession: {},
          models: [],
        },
      ),
    ).toMatchObject({
      command: {
        type: "session.invokeSlashCommand",
        data: { invocation: { invocation: "/compact" } },
      },
    });
    expect(() =>
      encodeClientCommand(
        {
          id: "command-empty-slash",
          type: "session.invokeSlashCommand",
          sessionId: "session-demo",
          invocation: "/ ",
        },
        {
          hostId: "host-demo",
          deviceId: "device-browser",
          issuedAtMs: 1_721_000_000_062,
          actorGenerationBySession: { "session-demo": 3 },
          modelIdBySession: {},
          models: [],
        },
      ),
    ).toThrow(/slash-prefixed invocation is required/);
  });

  it("accepts provider-defined bounded reasoning from the host catalog", () => {
    const golden = clone(hostBootstrapGolden);
    golden.models[0]!.reasoning = ["off", "on", "budget=8192"];
    golden.models[0]!.defaultReasoning = "budget=8192";
    golden.sessions[0]!.model.reasoning = "budget=8192";
    golden.selectedSession.model.reasoning = "budget=8192";

    const { bootstrap, selectedSession } = projectHostBootstrap(golden);
    if (!selectedSession) {
      throw new Error("provider bootstrap must select a session");
    }
    expect(bootstrap.models[0]?.reasoning).toEqual([
      "off",
      "on",
      "budget=8192",
    ]);
    expect(selectedSession.reasoning).toBe("budget=8192");
  });

  it("normalizes structured pull-request evidence and rejects unknown states", () => {
    const inProgress = clone(hostBootstrapGolden);
    inProgress.sessions[0]!.pullRequest.state = "inProgress";
    expect(
      projectHostBootstrap(inProgress).bootstrap.sessions[0]?.pullRequest,
    ).toEqual({ state: "in_progress" });

    const invalid = clone(hostBootstrapGolden);
    invalid.sessions[0]!.pullRequest.state = "draft";
    expect(() => projectHostBootstrap(invalid)).toThrow(WireContractError);
  });

  it("rejects unknown wire fields and catalog-invalid selections", () => {
    expect(() =>
      projectHostBootstrap({
        ...hostBootstrapGolden,
        injected: true,
      }),
    ).toThrow(WireContractError);

    const invalid = clone(hostBootstrapGolden);
    invalid.selectedSession.model.reasoning = "provider-secret";
    expect(() => projectHostBootstrap(invalid)).toThrow(
      /is not advertised by the selected model/,
    );
  });

  it("fails honestly for approval scopes absent from the Rust contract", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };

    expect(() =>
      encodeClientCommand(
        {
          id: "command-approval",
          type: "approval.resolve",
          sessionId: "session-demo",
          requestId: "request-1",
          decision: "allowed_session",
        },
        context,
      ),
    ).toThrow(/one-shot approval only/);
  });
});

describe("usage wire projections", () => {
  const totals = {
    prompt_tokens: 120,
    completion_tokens: 80,
    cache_read_tokens: 40,
    cache_write_tokens: 20,
    cache_write_1h_tokens: 5,
    reasoning_tokens: 16,
    total_tokens: 260,
    request_count: 3,
    models: [
      {
        provider: "anthropic",
        model: "claude-sonnet-4-6",
        prompt_tokens: 120,
        completion_tokens: 80,
        cache_read_tokens: 40,
        cache_write_tokens: 20,
        cache_write_1h_tokens: 5,
        reasoning_tokens: 16,
        total_tokens: 260,
        request_count: 3,
      },
    ],
    models_truncated: false,
  };

  it("projects exact token buckets and nullable lifetime timestamps", () => {
    expect(projectUsageStats({ period: "weekly", ...totals })).toEqual({
      period: "weekly",
      promptTokens: 120,
      completionTokens: 80,
      cacheReadTokens: 40,
      cacheWriteTokens: 20,
      cacheWriteOneHourTokens: 5,
      reasoningTokens: 16,
      totalTokens: 260,
      requestCount: 3,
      models: [
        {
          provider: "anthropic",
          model: "claude-sonnet-4-6",
          promptTokens: 120,
          completionTokens: 80,
          cacheReadTokens: 40,
          cacheWriteTokens: 20,
          cacheWriteOneHourTokens: 5,
          reasoningTokens: 16,
          totalTokens: 260,
          requestCount: 3,
        },
      ],
      modelsTruncated: false,
    });
    expect(
      projectLifetimeUsage({
        ...totals,
        first_request_at_ms: null,
        last_request_at_ms: null,
      }),
    ).toMatchObject({
      firstRequestAtMs: undefined,
      lastRequestAtMs: undefined,
    });
  });

  it("requires bounded, unique, descending model breakdowns", () => {
    const model = totals.models[0];
    expect(() =>
      projectUsageStats({
        period: "daily",
        ...totals,
        models: [model, { ...model }],
      }),
    ).toThrow(/unique model/);
    expect(() =>
      projectUsageStats({
        period: "daily",
        ...totals,
        models: [
          { ...model, total_tokens: 1 },
          { ...model, model: "claude-opus-4-6", total_tokens: 2 },
        ],
      }),
    ).toThrow(/ordered by total tokens/);
    expect(() =>
      projectUsageStats({
        period: "daily",
        ...totals,
        models: Array.from({ length: 257 }, (_, index) => ({
          ...model,
          model: `model-${index}`,
          total_tokens: 257 - index,
        })),
      }),
    ).toThrow(/at most 256 models/);
  });

  it("projects ordered daily activity and rejects malformed contracts", () => {
    expect(
      projectUsageActivity({
        days: [
          { date: "2025-01-01", tokens: 100, request_count: 1 },
          { date: "2025-01-03", tokens: 300, request_count: 2 },
        ],
        current_streak: 1,
        longest_streak: 4,
      }),
    ).toEqual({
      days: [
        { date: "2025-01-01", tokens: 100, requestCount: 1 },
        { date: "2025-01-03", tokens: 300, requestCount: 2 },
      ],
      currentStreak: 1,
      longestStreak: 4,
    });

    expect(() =>
      projectUsageStats({ period: "monthly", ...totals }),
    ).toThrow(WireContractError);
    expect(() =>
      projectUsageStats({ period: "daily", ...totals, injected: true }),
    ).toThrow(WireContractError);
    expect(() =>
      projectUsageActivity({
        days: [
          { date: "2025-02-30", tokens: 1, request_count: 1 },
        ],
        current_streak: 1,
        longest_streak: 1,
      }),
    ).toThrow(WireContractError);
    expect(() =>
      projectUsageActivity({
        days: [
          { date: "2025-01-03", tokens: 1, request_count: 1 },
          { date: "2025-01-02", tokens: 1, request_count: 1 },
        ],
        current_streak: 1,
        longest_streak: 1,
      }),
    ).toThrow(/ordered oldest first/);
  });
});
