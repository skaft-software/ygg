import {
  fixtureBootstrap,
  fixtureSessions,
} from "./fixtures";
import type {
  ClientCommand,
  CommandAck,
  HostBootstrap,
  SessionEvent,
  SessionSnapshot,
} from "./protocol";

type EventListener = (event: SessionEvent) => void;

export interface YggTransport {
  connect(): Promise<HostBootstrap>;
  getSession(sessionId: string, signal?: AbortSignal): Promise<SessionSnapshot>;
  send(command: ClientCommand): Promise<CommandAck>;
  subscribe(listener: EventListener): () => void;
  close(): void;
}

const clone = <T,>(value: T): T => structuredClone(value);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// The wire contract is deliberately contained here. The Rust golden payload
// integration can replace these projectors without leaking wire naming into UI.
function decodeBootstrap(value: unknown): HostBootstrap {
  if (
    !isRecord(value) ||
    !isRecord(value.host) ||
    !Array.isArray(value.sessions) ||
    !Array.isArray(value.projects) ||
    !Array.isArray(value.models) ||
    !Array.isArray(value.themes) ||
    typeof value.selectedSessionId !== "string"
  ) {
    throw new Error("Unsupported Ygg bootstrap payload.");
  }
  return value as unknown as HostBootstrap;
}

function decodeSession(value: unknown): SessionSnapshot {
  if (
    !isRecord(value) ||
    typeof value.sessionId !== "string" ||
    typeof value.sequence !== "number" ||
    !Array.isArray(value.items)
  ) {
    throw new Error("Unsupported Ygg session payload.");
  }
  return value as unknown as SessionSnapshot;
}

function decodeAck(value: unknown): CommandAck {
  if (
    !isRecord(value) ||
    typeof value.commandId !== "string" ||
    typeof value.accepted !== "boolean"
  ) {
    throw new Error("Unsupported Ygg command acknowledgement.");
  }
  return value as unknown as CommandAck;
}

function decodeEvent(value: unknown): SessionEvent {
  if (
    !isRecord(value) ||
    typeof value.type !== "string" ||
    typeof value.sessionId !== "string" ||
    typeof value.sequence !== "number"
  ) {
    throw new Error("Unsupported Ygg event payload.");
  }
  return value as unknown as SessionEvent;
}

function encodeCommand(command: ClientCommand): unknown {
  return command;
}

export class FixtureTransport implements YggTransport {
  private bootstrap = clone(fixtureBootstrap);
  private sessions = clone(fixtureSessions);
  private listeners = new Set<EventListener>();
  private timers = new Set<number>();
  private createdCount = 0;

  async connect(): Promise<HostBootstrap> {
    return clone(this.bootstrap);
  }

  async getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
    const snapshot = this.sessions[sessionId];
    if (!snapshot) {
      throw new Error(`Unknown fixture session ${sessionId}`);
    }
    return clone(snapshot);
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  close(): void {
    for (const timer of this.timers) {
      window.clearTimeout(timer);
    }
    this.timers.clear();
    this.listeners.clear();
  }

  private emit(event: SessionEvent): void {
    const current = this.sessions[event.sessionId];
    if (current) {
      if (event.type === "session.snapshot") {
        this.sessions[event.sessionId] = clone(event.snapshot);
      } else if (event.type === "session.updated") {
        this.sessions[event.sessionId] = {
          ...current,
          ...event.patch,
          sequence: event.sequence,
        };
      } else if (event.type === "item.started") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: [...current.items, clone(event.item)],
        };
      } else if (event.type === "item.delta") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.map((item) => {
            if (item.id !== event.itemId) return item;
            if (event.field === "content" && "content" in item) {
              return { ...item, content: `${item.content}${event.delta}` };
            }
            if (event.field === "detail" && item.kind === "action") {
              return { ...item, detail: `${item.detail ?? ""}${event.delta}` };
            }
            return item;
          }),
        };
      } else if (event.type === "item.committed") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.map((item) =>
            item.id === event.item.id ? clone(event.item) : item,
          ),
        };
      } else if (event.type === "item.retracted") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.filter((item) => item.id !== event.itemId),
        };
      } else if (event.type === "session.resources") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          progress: clone(event.progress ?? current.progress),
          sources: clone(event.sources ?? current.sources),
          outputs: clone(event.outputs ?? current.outputs),
          previews: clone(event.previews ?? current.previews),
        };
      }
    }

    for (const listener of this.listeners) {
      listener(clone(event));
    }
  }

  private later(delay: number, callback: () => void): void {
    const timer = window.setTimeout(() => {
      this.timers.delete(timer);
      callback();
    }, delay);
    this.timers.add(timer);
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    if (command.type === "theme.select") {
      if (!this.bootstrap.themes.some((theme) => theme.id === command.themeId)) {
        return {
          commandId: command.id,
          accepted: false,
          error: "Theme is not available.",
        };
      }
      this.bootstrap.selectedThemeId = command.themeId;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.create") {
      this.createdCount += 1;
      const sessionId = `session-created-${this.createdCount}`;
      const now = new Date().toISOString();
      const snapshot: SessionSnapshot = {
        sessionId,
        actorGeneration: 1,
        sequence: 1,
        title: "New session",
        status: "idle",
        projectId: command.projectId,
        modelId: command.modelId,
        reasoning: command.reasoning,
        authority: command.authority,
        contextPercent: 0,
        startedAt: now,
        items: [],
        progress: [],
        sources: [],
        outputs: [],
        previews: [],
      };
      this.sessions[sessionId] = snapshot;
      this.bootstrap.sessions.unshift({
        id: sessionId,
        projectId: command.projectId,
        title: "New session",
        preview: "Ready when you are",
        status: "idle",
        updatedAt: now,
        pinned: false,
        archived: false,
        unread: false,
        modelId: command.modelId,
        attentionCount: 0,
      });
      return {
        commandId: command.id,
        accepted: true,
        createdSessionId: sessionId,
      };
    }

    const snapshot = this.sessions[command.sessionId];
    if (!snapshot) {
      return {
        commandId: command.id,
        accepted: false,
        error: "Session is not available.",
      };
    }

    if (command.type === "session.submit") {
      const turnId = `turn-${snapshot.sequence + 1}`;
      let sequence = snapshot.sequence + 1;
      const now = new Date().toISOString();
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: sequence++,
        patch: {
          status: "working",
          title:
            snapshot.items.length === 0
              ? command.prompt.slice(0, 52)
              : snapshot.title,
        },
      });
      this.emit({
        type: "item.started",
        sessionId: command.sessionId,
        sequence: sequence++,
        item: {
          id: `${turnId}-user`,
          turnId,
          kind: "user_message",
          content: command.prompt,
          attachments: command.attachments,
          state: "committed",
          createdAt: now,
        },
      });

      const reasoningId = `${turnId}-reasoning`;
      this.later(260, () => {
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: reasoningId,
            turnId,
            kind: "reasoning",
            summary: "Understanding the request",
            content: "",
            state: "streaming",
            createdAt: new Date().toISOString(),
          },
        });
      });
      this.later(580, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: reasoningId,
          field: "content",
          delta:
            "I’m grounding the request in the current project and checking the most direct path.",
        });
      });
      this.later(1_150, () => {
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: reasoningId,
            turnId,
            kind: "reasoning",
            summary: "Request understood",
            content:
              "I grounded the request in the current project and selected the smallest complete path.",
            state: "committed",
            createdAt: now,
          },
        });
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: `${turnId}-action`,
            turnId,
            kind: "action",
            actionKind: "analysis",
            label: "Inspected the project context",
            detail: "Found the relevant session and project state.",
            state: "committed",
            createdAt: new Date().toISOString(),
          },
        });
      });

      const assistantId = `${turnId}-assistant`;
      this.later(1_650, () => {
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: assistantId,
            turnId,
            kind: "assistant_message",
            content: "",
            state: "streaming",
            createdAt: new Date().toISOString(),
          },
        });
      });
      this.later(1_900, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: assistantId,
          field: "content",
          delta: "I’ve got it. ",
        });
      });
      this.later(2_180, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: assistantId,
          field: "content",
          delta:
            "The session is connected, the request is grounded, and I’m ready to continue with the real Ygg runtime.",
        });
      });
      this.later(2_650, () => {
        const content =
          "I’ve got it. The session is connected, the request is grounded, and I’m ready to continue with the real Ygg runtime.";
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: assistantId,
            turnId,
            kind: "assistant_message",
            content,
            state: "committed",
            createdAt: now,
          },
        });
        this.emit({
          type: "session.updated",
          sessionId: command.sessionId,
          sequence: sequence++,
          patch: { status: "done", contextPercent: 4 },
        });
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: `${turnId}-outcome`,
            turnId,
            kind: "run_outcome",
            outcome: "done",
            durationMs: 2_650,
            summary: "Request completed",
            state: "committed",
            createdAt: new Date().toISOString(),
          },
        });
      });

      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.interrupt") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: { status: "stopped" },
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.configure") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: {
          modelId: command.modelId,
          reasoning: command.reasoning,
          authority: command.authority,
        },
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.rename") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: { title: command.title },
      });
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.title = command.title;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.pin") {
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.pinned = command.pinned;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.archive") {
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.archived = command.archived;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "approval.resolve") {
      const item = snapshot.items.find(
        (candidate) =>
          candidate.kind === "approval" &&
          candidate.requestId === command.requestId,
      );
      if (item?.kind === "approval") {
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 1,
          item: {
            ...item,
            resolved: command.decision,
            state: "committed",
          },
        });
        this.emit({
          type: "session.updated",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 2,
          patch: {
            status: command.decision === "denied" ? "stopped" : "working",
          },
        });
      }
      return { commandId: command.id, accepted: true };
    }

    throw new Error("Unsupported fixture command.");
  }
}

export class HttpTransport implements YggTransport {
  private listeners = new Set<EventListener>();
  private socket: WebSocket | null = null;

  async connect(): Promise<HostBootstrap> {
    const response = await fetch("/api/bootstrap", {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    });
    if (!response.ok) {
      throw new Error(`Bootstrap failed with ${response.status}`);
    }
    const bootstrap = decodeBootstrap(await response.json());
    this.openSocket();
    return bootstrap;
  }

  async getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    const response = await fetch(
      `/api/sessions/${encodeURIComponent(sessionId)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        signal,
      },
    );
    if (!response.ok) {
      throw new Error(`Session failed with ${response.status}`);
    }
    return decodeSession(await response.json());
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    const response = await fetch("/api/commands", {
      method: "POST",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(encodeCommand(command)),
    });
    if (!response.ok) {
      throw new Error(`Command failed with ${response.status}`);
    }
    return decodeAck(await response.json());
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  close(): void {
    this.socket?.close();
    this.socket = null;
    this.listeners.clear();
  }

  private openSocket(): void {
    const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
    this.socket = new WebSocket(`${scheme}//${window.location.host}/api/events`);
    this.socket.addEventListener("message", (message) => {
      const event = decodeEvent(JSON.parse(String(message.data)));
      for (const listener of this.listeners) {
        listener(event);
      }
    });
  }
}

export function createTransport(): YggTransport {
  const params = new URLSearchParams(window.location.search);
  const forceHttp = params.get("transport") === "http";
  // Live transport remains opt-in until this projector passes the Rust golden
  // contract fixtures. Production mode alone must not imply compatibility.
  if (forceHttp) {
    return new HttpTransport();
  }
  return new FixtureTransport();
}
