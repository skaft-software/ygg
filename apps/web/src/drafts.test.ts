import { describe, expect, it, vi } from "vitest";
import type { AttachmentRef } from "./protocol";
import {
  type DraftStorage,
  SessionDraftStore,
} from "./drafts";

function memoryStorage(): DraftStorage {
  const values = new Map<string, string>();
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
  };
}

const attachment: AttachmentRef = {
  id: "upload-one",
  handle: "upload:one",
  name: "diagram.png",
  mediaType: "image/png",
  size: 42,
};

describe("session draft store", () => {
  it("keeps drafts isolated by host and session across store instances", () => {
    const storage = memoryStorage();
    const writer = new SessionDraftStore(storage);
    expect(
      writer.save("host-a", "session-a", {
        text: "Draft A",
        delivery: "submit",
        attachments: [attachment],
        updatedAt: "2026-07-27T03:00:00Z",
      }),
    ).toBe(true);
    expect(
      writer.save("host-a", "session-b", {
        text: "Draft B",
        delivery: "followUp",
        attachments: [],
        updatedAt: "2026-07-27T03:01:00Z",
      }),
    ).toBe(true);

    const reader = new SessionDraftStore(storage);
    expect(reader.load("host-a", "session-a")).toEqual({
      text: "Draft A",
      delivery: "submit",
      attachments: [attachment],
      updatedAt: "2026-07-27T03:00:00.000Z",
    });
    expect(reader.load("host-a", "session-b")?.text).toBe("Draft B");
    expect(reader.load("host-b", "session-a")).toBeNull();
  });

  it("clears only the acknowledged session draft", () => {
    const storage = memoryStorage();
    const drafts = new SessionDraftStore(storage);
    for (const sessionId of ["session-a", "session-b"]) {
      drafts.save("host", sessionId, {
        text: sessionId,
        delivery: "steer",
        attachments: [],
        updatedAt: "2026-07-27T03:00:00Z",
      });
    }

    drafts.clear("host", "session-a");
    expect(drafts.load("host", "session-a")).toBeNull();
    expect(drafts.load("host", "session-b")?.text).toBe("session-b");
  });

  it("rejects oversized or structurally invalid drafts", () => {
    const drafts = new SessionDraftStore(memoryStorage());
    expect(
      drafts.save("host", "session", {
        text: "x".repeat(256 * 1024 + 1),
        delivery: "submit",
        attachments: [],
        updatedAt: "2026-07-27T03:00:00Z",
      }),
    ).toBe(false);
    expect(
      drafts.save("host", "session", {
        text: "safe",
        delivery: "submit",
        attachments: [{ ...attachment, size: Number.NaN }],
        updatedAt: "2026-07-27T03:00:00Z",
      }),
    ).toBe(false);
    expect(
      drafts.save("host", "session", {
        text: "safe",
        delivery: "submit",
        attachments: [],
        updatedAt: "not-a-date",
      }),
    ).toBe(false);
  });

  it("falls back in memory when browser storage is unavailable", () => {
    const storage: DraftStorage = {
      getItem: vi.fn(() => {
        throw new Error("blocked");
      }),
      setItem: vi.fn(() => {
        throw new Error("blocked");
      }),
      removeItem: vi.fn(() => {
        throw new Error("blocked");
      }),
    };
    const drafts = new SessionDraftStore(storage);
    expect(
      drafts.save("host", "session", {
        text: "kept for this page",
        delivery: "submit",
        attachments: [],
        updatedAt: "2026-07-27T03:00:00Z",
      }),
    ).toBe(true);
    expect(drafts.load("host", "session")?.text).toBe(
      "kept for this page",
    );
  });
});
