import { describe, expect, it } from "vitest";
import type {
  ActionItem,
  AssistantMessageItem,
  SessionSnapshot,
} from "./protocol";
import { reduceSessionEvent, SessionSequenceGapError } from "./reducer";

const base: SessionSnapshot = {
  sessionId: "session-1",
  actorGeneration: 1,
  sequence: 4,
  title: "Test session",
  status: "working",
  projectId: "project-1",
  modelId: "model-1",
  reasoning: "medium",
  authority: "readOnly",
  contextPercent: 12,
  startedAt: "2026-07-26T12:00:00.000Z",
  branches: { entries: [], truncated: false },
  items: [],
  progress: [],
  sources: [],
  outputs: [],
  previews: [],
};

describe("reduceSessionEvent", () => {
  it("reconciles a streaming item with its authoritative completion", () => {
    const started: AssistantMessageItem = {
      id: "assistant-1",
      turnId: "turn-1",
      kind: "assistant_message",
      content: "I am",
      state: "streaming",
      createdAt: "2026-07-26T12:00:01.000Z",
    };

    const streaming = reduceSessionEvent(base, {
      type: "item.started",
      sessionId: "session-1",
      sequence: 5,
      item: started,
    });
    const withDelta = reduceSessionEvent(streaming, {
      type: "item.delta",
      sessionId: "session-1",
      sequence: 6,
      itemId: "assistant-1",
      field: "content",
      delta: " working.",
    });
    const committed = reduceSessionEvent(withDelta, {
      type: "item.committed",
      sessionId: "session-1",
      sequence: 7,
      item: {
        ...started,
        content: "I am working.",
        state: "committed",
      },
    });

    expect(committed.items).toHaveLength(1);
    expect(committed.items[0]).toMatchObject({
      id: "assistant-1",
      content: "I am working.",
      state: "committed",
    });
  });

  it("ignores duplicate and stale events", () => {
    const next = reduceSessionEvent(base, {
      type: "session.updated",
      sessionId: "session-1",
      sequence: 4,
      patch: { title: "Stale title" },
    });

    expect(next).toBe(base);
  });

  it("uses an authoritative snapshot after a replay gap", () => {
    const replacement = {
      ...base,
      sequence: 30,
      status: "done" as const,
      title: "Recovered",
    };

    const next = reduceSessionEvent(base, {
      type: "session.snapshot",
      sessionId: "session-1",
      sequence: 30,
      snapshot: replacement,
    });

    expect(next).toEqual(replacement);
  });

  it("replaces every projected collection without retaining the old branch", () => {
    const oldItem: AssistantMessageItem = {
      id: "old-item",
      turnId: "old-turn",
      kind: "assistant_message",
      content: "Old branch",
      state: "committed",
      createdAt: base.startedAt,
    };
    const current = {
      ...base,
      items: [oldItem],
      sources: [
        {
          id: "old-source",
          kind: "file" as const,
          title: "old.ts",
          subtitle: "Old",
          consultedAt: base.startedAt,
          iconLabel: "TS",
        },
      ],
      branches: {
        head: "entry-old",
        entries: [
          {
            entryId: "entry-old",
            kind: "assistantMessage" as const,
            checkoutable: true,
            label: "Old branch",
          },
        ],
        truncated: false,
      },
    };
    const replacement = {
      ...base,
      sequence: 5,
      status: "idle" as const,
      items: [],
      sources: [],
      outputs: [],
      previews: [],
      progress: [],
      branches: {
        head: "entry-root",
        entries: [
          {
            entryId: "entry-root",
            kind: "userMessage" as const,
            checkoutable: true,
            label: "Root",
          },
        ],
        truncated: false,
      },
    };

    const next = reduceSessionEvent(current, {
      type: "session.snapshot",
      sessionId: "session-1",
      actorGeneration: 1,
      sequence: 5,
      snapshot: replacement,
    });

    expect(next).toEqual(replacement);
    expect(next.items).toEqual([]);
    expect(next.sources).toEqual([]);
    expect(next.branches.entries.map((entry) => entry.entryId)).toEqual([
      "entry-root",
    ]);
  });

  it("accepts a higher actor generation with a reset sequence and ignores older generations", () => {
    const nextGeneration = {
      ...base,
      actorGeneration: 2,
      sequence: 1,
      title: "New owner",
    };
    const replaced = reduceSessionEvent(base, {
      type: "session.snapshot",
      sessionId: "session-1",
      actorGeneration: 2,
      sequence: 1,
      snapshot: nextGeneration,
    });
    expect(replaced).toEqual(nextGeneration);

    const stale = reduceSessionEvent(replaced, {
      type: "session.updated",
      sessionId: "session-1",
      actorGeneration: 1,
      sequence: 99,
      patch: { title: "Stale owner" },
    });
    expect(stale).toBe(replaced);
  });

  it("retracts only the rejected provisional item", () => {
    const assistant: AssistantMessageItem = {
      id: "assistant-retry",
      turnId: "turn-1",
      kind: "assistant_message",
      content: "Rejected candidate",
      state: "streaming",
      createdAt: "2026-07-26T12:00:01.000Z",
    };
    const snapshot = { ...base, items: [assistant] };

    const next = reduceSessionEvent(snapshot, {
      type: "item.retracted",
      sessionId: "session-1",
      sequence: 5,
      itemId: "assistant-retry",
    });

    expect(next.items).toEqual([]);
  });

  it("coalesces a tool result without losing its event sequence", () => {
    const action: ActionItem = {
      id: "tool-call-1",
      turnId: "turn-tool",
      kind: "action",
      actionKind: "command",
      label: "shell",
      detail: "Running tests",
      state: "streaming",
      createdAt: "2026-07-26T12:00:01.000Z",
    };
    const snapshot = { ...base, items: [action] };

    const next = reduceSessionEvent(snapshot, {
      type: "item.tool_result",
      sessionId: "session-1",
      sequence: 5,
      itemId: "tool-call-1",
      resultItemId: "tool-result-1",
      detail: "43 tests passed",
      state: "committed",
    });

    expect(next.sequence).toBe(5);
    expect(next.items).toEqual([
      expect.objectContaining({
        id: "tool-call-1",
        label: "shell",
        detail: "43 tests passed",
        state: "committed",
      }),
    ]);
  });

  it("detects a missing sequence so the store can replace from snapshot", () => {
    expect(() =>
      reduceSessionEvent(base, {
        type: "session.updated",
        sessionId: "session-1",
        sequence: 7,
        patch: { status: "done" },
      }),
    ).toThrow(SessionSequenceGapError);
  });

  it("rejects a snapshot whose inner identity does not match its envelope", () => {
    const next = reduceSessionEvent(base, {
      type: "session.snapshot",
      sessionId: "session-1",
      sequence: 8,
      snapshot: {
        ...base,
        sessionId: "session-other",
        sequence: 8,
      },
    });
    expect(next).toBe(base);
  });
});
