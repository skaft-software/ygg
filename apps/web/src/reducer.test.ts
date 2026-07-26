import { describe, expect, it } from "vitest";
import type {
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
  authority: "ask",
  contextPercent: 12,
  startedAt: "2026-07-26T12:00:00.000Z",
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
