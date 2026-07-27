import { describe, expect, it } from "vitest";
import type {
  ActionItem,
  AssistantMessageItem,
  SessionSnapshot,
} from "./protocol";
import {
  primeSessionItemIndex,
  reduceSessionEvent,
  reduceSessionEvents,
  SessionSequenceGapError,
} from "./reducer";

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

  it("coalesces adjacent streaming deltas without changing sequence semantics", () => {
    const started = reduceSessionEvent(base, {
      type: "item.started",
      sessionId: "session-1",
      sequence: 5,
      item: {
        id: "assistant-batch",
        turnId: "turn-batch",
        kind: "assistant_message",
        content: "Start",
        state: "streaming",
        createdAt: base.startedAt,
      },
    });

    const next = reduceSessionEvents(started, [
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 6,
        itemId: "assistant-batch",
        field: "content",
        delta: " one",
      },
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 7,
        itemId: "assistant-batch",
        field: "content",
        delta: " two",
      },
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 8,
        itemId: "assistant-batch",
        field: "content",
        delta: "Replacement",
        replace: true,
      },
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 9,
        itemId: "assistant-batch",
        field: "content",
        delta: " tail",
      },
    ]);

    expect(next.sequence).toBe(9);
    expect(next.items[0]).toMatchObject({
      id: "assistant-batch",
      content: "Replacement tail",
    });
  });

  it("uses the primed item index for a 1,000-item delta burst", () => {
    const items: AssistantMessageItem[] = Array.from(
      { length: 1_000 },
      (_, index) => ({
        id: `assistant-${index}`,
        turnId: `turn-${index}`,
        kind: "assistant_message",
        content: index === 999 ? "" : `Committed ${index}`,
        state: index === 999 ? "streaming" : "committed",
        createdAt: base.startedAt,
      }),
    );
    const snapshot: SessionSnapshot = {
      ...base,
      sequence: 10,
      items,
    };
    primeSessionItemIndex(snapshot);
    Object.defineProperties(items, {
      findIndex: {
        configurable: true,
        value: () => {
          throw new Error("delta performed a linear findIndex scan");
        },
      },
      map: {
        configurable: true,
        value: () => {
          throw new Error("delta mapped the full transcript");
        },
      },
    });

    const events = Array.from({ length: 60 }, (_, index) => ({
      type: "item.delta" as const,
      sessionId: snapshot.sessionId,
      sequence: snapshot.sequence + index + 1,
      itemId: "assistant-999",
      field: "content" as const,
      delta: `chunk-${index + 1} `,
    }));
    const next = reduceSessionEvents(snapshot, events);

    expect(next.sequence).toBe(70);
    expect(next.items[0]).toBe(items[0]);
    expect(next.items[998]).toBe(items[998]);
    expect(next.items[999]).toMatchObject({
      content: events.map((event) => event.delta).join(""),
    });
  });

  it("does not coalesce across another ordered event", () => {
    const started = reduceSessionEvent(base, {
      type: "item.started",
      sessionId: "session-1",
      sequence: 5,
      item: {
        id: "assistant-ordered",
        turnId: "turn-ordered",
        kind: "assistant_message",
        content: "",
        state: "streaming",
        createdAt: base.startedAt,
      },
    });

    const next = reduceSessionEvents(started, [
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 6,
        itemId: "assistant-ordered",
        field: "content",
        delta: "Before",
      },
      {
        type: "session.updated",
        sessionId: "session-1",
        sequence: 7,
        patch: { title: "Middle event" },
      },
      {
        type: "item.delta",
        sessionId: "session-1",
        sequence: 8,
        itemId: "assistant-ordered",
        field: "content",
        delta: " after",
      },
    ]);

    expect(next.sequence).toBe(8);
    expect(next.title).toBe("Middle event");
    expect(next.items[0]).toMatchObject({ content: "Before after" });
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
      phase: "verified",
      status: "running",
      rawToolName: "bash",
      label: "shell",
      detail: "Running tests",
      observedOutputBytes: 0,
      droppedOutputBytes: 0,
      changedPaths: [],
      sourceIds: [],
      outputIds: [],
      state: "streaming",
      createdAt: "2026-07-26T12:00:01.000Z",
    };
    const snapshot = { ...base, items: [action] };

    const next = reduceSessionEvent(snapshot, {
      type: "item.activity_result",
      sessionId: "session-1",
      sequence: 5,
      itemId: "tool-call-1",
      resultItemId: "tool-result-1",
      result: {
        status: "succeeded",
        summary: "43 tests passed",
        completedAt: "2026-07-26T12:00:02.000Z",
        durationMs: 1_000,
        outputSummary: "43 tests passed",
        observedOutputBytes: 1_024,
        droppedOutputBytes: 0,
      },
    });

    expect(next.sequence).toBe(5);
    expect(next.items).toEqual([
      expect.objectContaining({
        id: "tool-call-1",
        label: "shell",
        detail: "43 tests passed",
        status: "succeeded",
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
