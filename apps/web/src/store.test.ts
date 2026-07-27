import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap, fixtureSessions } from "./fixtures";
import type {
  ClientCommand,
  CommandAck,
  AttachmentRef,
  HostEvent,
  HostBootstrap,
  SessionSnapshot,
} from "./protocol";
import { sessionIdFromPathname, YggStore } from "./store";
import type { YggTransport } from "./transport";

type SessionLoader = (
  sessionId: string,
  signal?: AbortSignal,
) => Promise<SessionSnapshot>;

const clone = <T,>(value: T): T => structuredClone(value);

class TestTransport implements YggTransport {
  readonly commands: ClientCommand[] = [];
  readonly listeners = new Set<(event: HostEvent) => void>();
  sessionLoader: SessionLoader = async (sessionId) => {
    const snapshot = fixtureSessions[sessionId];
    if (!snapshot) throw new Error(`Unknown session ${sessionId}`);
    return clone(snapshot);
  };
  commandHandler: (command: ClientCommand) => Promise<CommandAck> = async (
    command,
  ) => ({ commandId: command.id, accepted: true });

  async connect(): Promise<HostBootstrap> {
    return clone(fixtureBootstrap);
  }

  getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    return this.sessionLoader(sessionId, signal);
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    this.commands.push(clone(command));
    return this.commandHandler(command);
  }

  async ingestAttachment(file: File): Promise<AttachmentRef> {
    return {
      id: "test-attachment",
      handle: "test-attachment",
      name: file.name,
      mediaType: file.type,
      size: file.size,
    };
  }

  attachmentContentUrl(handle: string): string {
    return `/api/v1/attachments/${encodeURIComponent(handle)}`;
  }

  resourceContentUrl(sessionId: string, handle: string): string {
    return `/api/v1/sessions/${encodeURIComponent(sessionId)}/resources/${encodeURIComponent(handle)}`;
  }

  subscribe(listener: (event: HostEvent) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  emit(event: HostEvent): void {
    for (const listener of this.listeners) listener(clone(event));
  }

  close(): void {
    this.listeners.clear();
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}

async function nextFrame() {
  await new Promise<void>((resolve) => window.setTimeout(resolve, 0));
}

beforeEach(() => {
  vi.stubGlobal(
    "requestAnimationFrame",
    (callback: FrameRequestCallback) =>
      window.setTimeout(() => callback(performance.now()), 0),
  );
  vi.stubGlobal("cancelAnimationFrame", (handle: number) =>
    window.clearTimeout(handle),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("YggStore", () => {
  it("retries a transport failure with the same command identity", async () => {
    const transport = new TestTransport();
    let attempt = 0;
    transport.commandHandler = async (command) => {
      attempt += 1;
      if (attempt === 1) throw new Error("connection reset");
      return { commandId: command.id, accepted: true };
    };
    const store = new YggStore(transport);
    await store.initialize();

    await store.submit("Keep the command id stable", []);

    expect(transport.commands).toHaveLength(2);
    expect(transport.commands[0].id).toBe(transport.commands[1].id);
    expect(transport.commands[0]).toEqual(transport.commands[1]);
    store.dispose();
  });

  it("does not let a stale session load replace a newer selection", async () => {
    const transport = new TestTransport();
    const delayed = deferred<SessionSnapshot>();
    let delayedSignal: AbortSignal | undefined;
    transport.sessionLoader = async (sessionId, signal) => {
      if (sessionId === "session-live") {
        delayedSignal = signal;
        return delayed.promise;
      }
      return clone(fixtureSessions[sessionId]);
    };
    const store = new YggStore(transport);
    await store.initialize();

    const oldSelection = store.selectSession("session-live");
    await store.selectSession("session-done");
    delayed.resolve(clone(fixtureSessions["session-live"]));
    await oldSelection;

    expect(delayedSignal?.aborted).toBe(true);
    expect(store.getSnapshot().selectedSessionId).toBe("session-done");
    store.dispose();
  });

  it("batches ordered events into one rendered store publication", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();
    let publications = 0;
    const unsubscribe = store.subscribe(() => {
      publications += 1;
    });

    transport.emit({
      type: "session.updated",
      sessionId: "session-fresh",
      sequence: 2,
      patch: { status: "working" },
    });
    transport.emit({
      type: "session.updated",
      sessionId: "session-fresh",
      sequence: 3,
      patch: { title: "Batched title" },
    });
    await nextFrame();

    expect(publications).toBe(1);
    expect(store.selectedSession).toMatchObject({
      sequence: 3,
      status: "working",
      title: "Batched title",
    });
    unsubscribe();
    store.dispose();
  });

  it("inserts and refreshes background summaries from other clients", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();

    transport.emit({
      type: "catalog.summary",
      catalogRevision: fixtureBootstrap.catalogRevision + 1,
      summary: {
        id: "session-phone",
        projectId: "project-ygg",
        title: "Started on the phone",
        preview: "Completed",
        status: "done",
        updatedAt: new Date().toISOString(),
        pinned: false,
        archived: false,
        unread: true,
        modelId: "gpt-5.6",
        attentionCount: 0,
      },
    });
    await nextFrame();

    expect(store.getSnapshot().bootstrap).toMatchObject({
      catalogRevision: fixtureBootstrap.catalogRevision + 1,
      sessions: expect.arrayContaining([
        expect.objectContaining({
          id: "session-phone",
          unread: true,
          status: "done",
        }),
      ]),
    });

    transport.emit({
      type: "catalog.summary",
      catalogRevision: fixtureBootstrap.catalogRevision + 2,
      summary: {
        id: "session-phone",
        projectId: "project-ygg",
        title: "Started on the phone",
        preview: "Needs your input",
        status: "needs_attention",
        updatedAt: new Date().toISOString(),
        pinned: false,
        archived: false,
        unread: false,
        modelId: "gpt-5.6",
        attentionCount: 1,
      },
    });
    await nextFrame();

    expect(
      store
        .getSnapshot()
        .bootstrap?.sessions.find((summary) => summary.id === "session-phone"),
    ).toMatchObject({
      status: "needs_attention",
      attentionCount: 1,
      unread: false,
    });
    store.dispose();
  });

  it("marks a background completion unread and attention states actionable", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();

    transport.emit({
      type: "session.updated",
      sessionId: "session-live",
      sequence: 19,
      patch: { status: "done" },
    });
    await nextFrame();
    expect(
      store
        .getSnapshot()
        .bootstrap?.sessions.find((summary) => summary.id === "session-live"),
    ).toMatchObject({ status: "done", unread: true, attentionCount: 0 });

    transport.emit({
      type: "session.updated",
      sessionId: "session-attention",
      sequence: 17,
      patch: { status: "needs_attention" },
    });
    await nextFrame();
    expect(
      store
        .getSnapshot()
        .bootstrap?.sessions.find(
          (summary) => summary.id === "session-attention",
        ),
    ).toMatchObject({
      status: "needs_attention",
      unread: false,
      attentionCount: 1,
    });
    store.dispose();
  });

  it("keeps needs-attention input on the active run", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();

    transport.emit({
      type: "session.updated",
      sessionId: "session-fresh",
      sequence: 2,
      patch: {
        status: "needs_attention",
        activeRunId: "run-live",
      },
    });
    await nextFrame();
    await store.submit("Here is the requested input", [], "followUp");

    expect(transport.commands.at(-1)).toMatchObject({
      type: "session.followUp",
      sessionId: "session-fresh",
    });
    store.dispose();
  });

  it("answers a typed user-input request through its owning session", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();

    await store.resolveUserInput("request-layout", {
      type: "choice",
      choice: "Compact",
    });

    expect(transport.commands.at(-1)).toMatchObject({
      type: "userInput.resolve",
      sessionId: "session-fresh",
      requestId: "request-layout",
      answer: { type: "choice", choice: "Compact" },
    });
    store.dispose();
  });

  it("serializes concurrent new-session requests without dropping either", async () => {
    const transport = new TestTransport();
    const firstAck = deferred<CommandAck>();
    let createCount = 0;
    transport.commandHandler = async (command) => {
      if (command.type !== "session.create") {
        return { commandId: command.id, accepted: true };
      }
      createCount += 1;
      if (createCount === 1) return firstAck.promise;
      return {
        commandId: command.id,
        accepted: true,
        createdSessionId: `session-created-${createCount}`,
      };
    };
    transport.sessionLoader = async (sessionId) => {
      if (!sessionId.startsWith("session-created-")) {
        return clone(fixtureSessions[sessionId]);
      }
      const selected = clone(fixtureSessions["session-fresh"]);
      return {
        ...selected,
        sessionId,
        title: "New session",
        projectId: "project-ygg",
        sequence: 0,
        items: [],
      };
    };
    const store = new YggStore(transport);
    await store.initialize();

    const first = store.createSession();
    const second = store.createSession();
    await nextFrame();
    expect(
      transport.commands.filter((command) => command.type === "session.create"),
    ).toHaveLength(1);

    firstAck.resolve({
      commandId: transport.commands[0]!.id,
      accepted: true,
      createdSessionId: "session-created-1",
    });
    await first;
    await second;

    expect(
      transport.commands.filter((command) => command.type === "session.create"),
    ).toHaveLength(2);
    expect(store.getSnapshot().bootstrap?.sessions).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ id: "session-created-1" }),
        expect.objectContaining({ id: "session-created-2" }),
      ]),
    );
    expect(store.getSnapshot().selectedSessionId).toBe("session-created-2");
    store.dispose();
  });

  it("refetches exactly once for projection replacement and never merges branches", async () => {
    const transport = new TestTransport();
    const oldItem = {
      id: "old-branch-item",
      turnId: "old-turn",
      kind: "assistant_message" as const,
      content: "Old branch",
      state: "committed" as const,
      createdAt: new Date().toISOString(),
    };
    const newItem = {
      ...oldItem,
      id: "new-branch-item",
      content: "Selected branch",
    };
    const oldSnapshot = {
      ...clone(fixtureSessions["session-fresh"]),
      sequence: 1,
      items: [oldItem],
      branches: {
        head: "entry-old",
        entries: [
          {
            entryId: "entry-root",
            kind: "userMessage" as const,
            checkoutable: true,
            label: "Root",
          },
          {
            entryId: "entry-old",
            parentEntryId: "entry-root",
            kind: "assistantMessage" as const,
            checkoutable: true,
            label: "Old branch",
          },
        ],
        truncated: false,
      },
    };
    const replacement = {
      ...oldSnapshot,
      sequence: 2,
      items: [newItem],
      sources: [],
      outputs: [],
      branches: { ...oldSnapshot.branches, head: "entry-root" },
    };
    let loads = 0;
    transport.sessionLoader = async () => {
      loads += 1;
      return clone(loads === 1 ? oldSnapshot : replacement);
    };
    const store = new YggStore(transport);
    await store.initialize();

    transport.emit({
      type: "session.projectionReplaced",
      sessionId: "session-fresh",
      actorGeneration: 1,
      sequence: 2,
      durableHead: "entry-root",
    });
    await nextFrame();
    await nextFrame();

    expect(loads).toBe(2);
    expect(store.selectedSession?.items.map((item) => item.id)).toEqual([
      "new-branch-item",
    ]);
    expect(store.selectedSession?.branches.head).toBe("entry-root");

    transport.emit({
      type: "session.projectionReplaced",
      sessionId: "session-fresh",
      actorGeneration: 1,
      sequence: 2,
      durableHead: "entry-root",
    });
    await nextFrame();
    expect(loads).toBe(2);
    store.dispose();
  });

  it("keeps retrying a replacement refetch until the required projection arrives", async () => {
    const transport = new TestTransport();
    const initial = {
      ...clone(fixtureSessions["session-fresh"]),
      sequence: 1,
    };
    const replacement = {
      ...initial,
      sequence: 2,
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
    let loads = 0;
    transport.sessionLoader = async () => {
      loads += 1;
      if (loads === 1) return clone(initial);
      if (loads === 2) throw new Error("snapshot is still being published");
      return clone(replacement);
    };
    const store = new YggStore(transport);
    await store.initialize();

    transport.emit({
      type: "session.projectionReplaced",
      sessionId: "session-fresh",
      actorGeneration: 1,
      sequence: 2,
      durableHead: "entry-root",
    });

    await vi.waitFor(() =>
      expect(store.selectedSession?.branches.head).toBe("entry-root"),
    );
    expect(loads).toBe(3);
    store.dispose();
  });

  it("sends checkout for a checkoutable checkpoint after work has finished", async () => {
    const transport = new TestTransport();
    const store = new YggStore(transport);
    await store.initialize();
    await store.selectSession("session-done");

    await store.checkoutBranch("entry-release-draft");

    expect(transport.commands.at(-1)).toMatchObject({
      type: "session.checkout",
      sessionId: "session-done",
      entryId: "entry-release-draft",
    });
    store.dispose();
  });

  it("rejects checkout while the host is disconnected", async () => {
    const transport = new TestTransport();
    transport.sessionLoader = async (sessionId) => ({
      ...clone(fixtureSessions[sessionId]),
      status: "disconnected",
    });
    const store = new YggStore(transport);
    await store.initialize();
    await store.selectSession("session-done");

    await expect(
      store.checkoutBranch("entry-release-draft"),
    ).rejects.toThrow("after current work finishes");
    expect(transport.commands).toHaveLength(0);
    store.dispose();
  });
});

describe("session routes", () => {
  it("decodes one explicit session route", () => {
    expect(sessionIdFromPathname("/session/session-demo")).toBe(
      "session-demo",
    );
    expect(sessionIdFromPathname("/session/a%20session/")).toBe("a session");
  });

  it("rejects root, nested, blank, and malformed session routes", () => {
    expect(sessionIdFromPathname("/")).toBeNull();
    expect(sessionIdFromPathname("/session/")).toBeNull();
    expect(sessionIdFromPathname("/session/a/extra")).toBeNull();
    expect(sessionIdFromPathname("/session/%2F")).toBeNull();
    expect(sessionIdFromPathname("/session/%E0%A4%A")).toBeNull();
  });
});
