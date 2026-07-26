import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap, fixtureSessions } from "./fixtures";
import type {
  ClientCommand,
  CommandAck,
  HostBootstrap,
  SessionEvent,
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
  readonly listeners = new Set<(event: SessionEvent) => void>();
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

  subscribe(listener: (event: SessionEvent) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  emit(event: SessionEvent): void {
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
