import {
  fixtureBootstrap,
  fixtureSessions,
} from "./fixtures";
import type {
  AttachmentRef,
  ClientCommand,
  CommandAck,
  HostBootstrap,
  ModelSummary,
  SessionEvent,
  SessionSnapshot,
  SessionSummary,
} from "./protocol";
import {
  decodeWireCommandAck,
  encodeClientCommand,
  projectHostBootstrap,
  projectHostStreamEvent,
  projectReplayResponse,
  projectSessionSnapshot,
} from "./wire";

type EventListener = (event: SessionEvent) => void;
export type TransportConnectionState =
  | "connecting"
  | "connected"
  | "reconnecting";
type ConnectionListener = (state: TransportConnectionState) => void;

export interface YggTransport {
  connect(selectedSessionId?: string): Promise<HostBootstrap>;
  getSession(sessionId: string, signal?: AbortSignal): Promise<SessionSnapshot>;
  send(command: ClientCommand): Promise<CommandAck>;
  ingestAttachment(file: File): Promise<AttachmentRef>;
  attachmentContentUrl(handle: string): string;
  subscribe(listener: EventListener): () => void;
  subscribeConnection?(listener: ConnectionListener): () => void;
  close(): void;
}

const clone = <T,>(value: T): T => structuredClone(value);

export class FixtureTransport implements YggTransport {
  private bootstrap = clone(fixtureBootstrap);
  private sessions = clone(fixtureSessions);
  private listeners = new Set<EventListener>();
  private timers = new Set<number>();
  private createdCount = 0;
  private attachmentFiles = new Map<string, File>();
  private attachmentUrls = new Map<string, string>();

  async connect(selectedSessionId?: string): Promise<HostBootstrap> {
    if (selectedSessionId) {
      if (!this.sessions[selectedSessionId]) {
        throw new Error(`Unknown fixture session ${selectedSessionId}`);
      }
      this.bootstrap.selectedSessionId = selectedSessionId;
    }
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

  subscribeConnection(listener: ConnectionListener): () => void {
    listener("connected");
    return () => {};
  }

  close(): void {
    for (const timer of this.timers) {
      window.clearTimeout(timer);
    }
    this.timers.clear();
    this.listeners.clear();
    for (const url of this.attachmentUrls.values()) URL.revokeObjectURL(url);
    this.attachmentFiles.clear();
    this.attachmentUrls.clear();
  }

  async ingestAttachment(file: File): Promise<AttachmentRef> {
    const identity = `${file.name}\0${file.type}\0${file.size}`;
    let hash = 2_166_136_261;
    for (let index = 0; index < identity.length; index += 1) {
      hash ^= identity.charCodeAt(index);
      hash = Math.imul(hash, 16_777_619);
    }
    const handle = `fixture-${(hash >>> 0).toString(16).padStart(8, "0")}`;
    this.attachmentFiles.set(handle, file);
    return {
      id: handle,
      handle,
      name: file.name,
      mediaType: file.type || "application/octet-stream",
      size: file.size,
    };
  }

  attachmentContentUrl(handle: string): string {
    const existing = this.attachmentUrls.get(handle);
    if (existing) return existing;
    const file = this.attachmentFiles.get(handle);
    if (!file) return "";
    const url = URL.createObjectURL(file);
    this.attachmentUrls.set(handle, url);
    return url;
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

    if (
      command.type === "session.submit" ||
      command.type === "session.steer" ||
      command.type === "session.followUp"
    ) {
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
              ? command.prompt.slice(0, 52) ||
                command.attachments[0]?.name ||
                "New session"
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
            "The session is connected, the request is grounded, and I’m ready to continue with the real ygg runtime.",
        });
      });
      this.later(2_650, () => {
        const content =
          "I’ve got it. The session is connected, the request is grounded, and I’m ready to continue with the real ygg runtime.";
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
          ...(command.modelId ? { modelId: command.modelId } : {}),
          ...(command.reasoning ? { reasoning: command.reasoning } : {}),
          ...(command.authority ? { authority: command.authority } : {}),
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
  private connectionListeners = new Set<ConnectionListener>();
  private connectionState: TransportConnectionState = "connecting";
  private socket: WebSocket | null = null;
  private reconnectTimer: number | null = null;
  private reconnectAttempt = 0;
  private closedByClient = false;
  private replaying = false;
  private bufferedEvents: Array<{
    hostSequence: number;
    event: SessionEvent;
  }> = [];
  private hostId: string | null = null;
  private models: ModelSummary[] = [];
  private summaries = new Map<string, SessionSummary>();
  private actorGenerationBySession: Record<string, number> = {};
  private modelIdBySession: Record<string, string> = {};
  private cursorBySession = new Map<
    string,
    { actorGeneration: number; sequence: number }
  >();
  private selectedSessionCache: SessionSnapshot | null = null;
  private encodedCommands = new Map<
    string,
    { hostScoped: boolean; body: string }
  >();

  constructor(private readonly deviceId?: string) {}

  async connect(selectedSessionId?: string): Promise<HostBootstrap> {
    this.closedByClient = false;
    const request: RequestInit = {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    };
    const response = selectedSessionId
      ? await fetch(
          `/api/v1/bootstrap?selectedSessionId=${encodeURIComponent(selectedSessionId)}`,
          request,
        )
      : await fetch("/api/v1/bootstrap", request);
    if (!response.ok) {
      throw new Error(`Bootstrap failed with ${response.status}`);
    }
    const { bootstrap, selectedSession } = projectHostBootstrap(
      await response.json(),
    );
    this.hostId = bootstrap.host.id;
    this.models = bootstrap.models;
    this.summaries = new Map(
      bootstrap.sessions.map((summary) => [summary.id, summary]),
    );
    this.selectedSessionCache = selectedSession;
    this.rememberSnapshot(selectedSession);
    this.openSocket();
    return bootstrap;
  }

  async getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    if (this.selectedSessionCache?.sessionId === sessionId) {
      const cached = this.selectedSessionCache;
      this.selectedSessionCache = null;
      return clone(cached);
    }
    const response = await fetch(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        signal,
      },
    );
    if (!response.ok) {
      throw new Error(`Session failed with ${response.status}`);
    }
    const snapshot = projectSessionSnapshot(await response.json(), {
      summary: this.summaries.get(sessionId),
      models: this.models,
    });
    this.rememberSnapshot(snapshot);
    return snapshot;
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    let encoded = this.encodedCommands.get(command.id);
    if (!encoded) {
      const envelope = encodeClientCommand(command, {
        hostId: this.hostId ?? "",
        deviceId: this.deviceId ?? "",
        issuedAtMs: Date.now(),
        actorGenerationBySession: this.actorGenerationBySession,
        modelIdBySession: this.modelIdBySession,
        models: this.models,
      });
      encoded = {
        hostScoped: command.type === "session.create",
        body: JSON.stringify(envelope),
      };
      this.encodedCommands.set(command.id, encoded);
    }

    const request: RequestInit = {
      method: "POST",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: encoded.body,
    };
    const response = encoded.hostScoped
      ? await fetch("/api/v1/commands/host", request)
      : await fetch("/api/v1/commands/session", request);
    if (!response.ok) {
      throw new Error(`Command failed with ${response.status}`);
    }
    const ack = decodeWireCommandAck(await response.json());
    if (ack.commandId !== command.id) {
      throw new Error("The host acknowledged a different command.");
    }
    this.encodedCommands.delete(command.id);
    if (
      command.type === "session.create" &&
      ack.accepted &&
      ack.createdSessionId
    ) {
      this.rememberCreatedSession(command, ack.createdSessionId);
    }
    return ack;
  }

  private rememberCreatedSession(
    command: Extract<ClientCommand, { type: "session.create" }>,
    sessionId: string,
  ): void {
    this.summaries.set(sessionId, {
      id: sessionId,
      projectId: command.projectId,
      title: "New session",
      preview: "Ready when you are",
      status: "idle",
      updatedAt: new Date().toISOString(),
      pinned: false,
      archived: false,
      unread: false,
      modelId: command.modelId,
      attentionCount: 0,
    });
    this.modelIdBySession[sessionId] = command.modelId;
  }

  async ingestAttachment(file: File): Promise<AttachmentRef> {
    const response = await fetch(
      `/api/v1/attachments?displayName=${encodeURIComponent(file.name)}`,
      {
        method: "POST",
        credentials: "same-origin",
        headers: {
          Accept: "application/json",
          "Content-Type": file.type || "application/octet-stream",
        },
        body: file,
      },
    );
    if (!response.ok) {
      throw new Error(`Attachment upload failed with ${response.status}`);
    }
    const value: unknown = await response.json();
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      throw new Error("Attachment upload returned an invalid response.");
    }
    const result = value as Record<string, unknown>;
    if (
      typeof result.handle !== "string" ||
      typeof result.displayName !== "string" ||
      typeof result.mediaType !== "string" ||
      typeof result.byteLen !== "number"
    ) {
      throw new Error("Attachment upload returned an invalid response.");
    }
    return {
      id: result.handle,
      handle: result.handle,
      name: result.displayName,
      mediaType: result.mediaType,
      size: result.byteLen,
    };
  }

  attachmentContentUrl(handle: string): string {
    return `/api/v1/attachments/${encodeURIComponent(handle)}`;
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  subscribeConnection(listener: ConnectionListener): () => void {
    this.connectionListeners.add(listener);
    listener(this.connectionState);
    return () => this.connectionListeners.delete(listener);
  }

  private setConnectionState(state: TransportConnectionState): void {
    if (this.connectionState === state) return;
    this.connectionState = state;
    for (const listener of this.connectionListeners) listener(state);
  }

  close(): void {
    this.closedByClient = true;
    if (this.reconnectTimer !== null) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.socket?.close();
    this.socket = null;
    this.bufferedEvents = [];
    this.listeners.clear();
    this.connectionListeners.clear();
  }

  private openSocket(): void {
    if (this.closedByClient) return;
    const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(
      `${scheme}//${window.location.host}/api/v1/events`,
    );
    this.socket = socket;

    socket.addEventListener("open", () => {
      this.reconnectAttempt = 0;
      this.setConnectionState("connected");
      this.replaying = true;
      void this.replayAll().then(
        () => {
          if (this.socket !== socket) return;
          this.replaying = false;
          const buffered = this.bufferedEvents
            .splice(0)
            .sort((left, right) => left.hostSequence - right.hostSequence);
          for (const projection of buffered) {
            this.dispatch(projection.event);
          }
        },
        () => {
          if (this.socket === socket) socket.close();
        },
      );
    });

    socket.addEventListener("message", (message) => {
      try {
        const projection = projectHostStreamEvent(
          JSON.parse(String(message.data)),
          { models: this.models },
        );
        this.rememberEvent(projection.event);
        if (this.replaying) {
          this.bufferedEvents.push(projection);
        } else {
          this.dispatch(projection.event);
        }
      } catch {
        socket.close(1002, "Invalid ygg event");
      }
    });

    socket.addEventListener("close", () => {
      if (this.socket === socket) this.socket = null;
      if (this.closedByClient) return;
      this.setConnectionState("reconnecting");
      const delay = Math.min(5_000, 250 * 2 ** this.reconnectAttempt);
      this.reconnectAttempt += 1;
      this.reconnectTimer = window.setTimeout(() => {
        this.reconnectTimer = null;
        this.openSocket();
      }, delay);
    });
  }

  private dispatch(event: SessionEvent): void {
    for (const listener of this.listeners) listener(event);
  }

  private rememberSnapshot(snapshot: SessionSnapshot): void {
    this.actorGenerationBySession[snapshot.sessionId] =
      snapshot.actorGeneration;
    this.modelIdBySession[snapshot.sessionId] = snapshot.modelId;
    this.cursorBySession.set(snapshot.sessionId, {
      actorGeneration: snapshot.actorGeneration,
      sequence: snapshot.sequence,
    });
  }

  private rememberEvent(event: SessionEvent): void {
    const generation = event.actorGeneration;
    if (generation !== undefined) {
      this.actorGenerationBySession[event.sessionId] = generation;
      this.cursorBySession.set(event.sessionId, {
        actorGeneration: generation,
        sequence: event.sequence,
      });
    }
    if (event.type === "session.snapshot") {
      this.rememberSnapshot(event.snapshot);
    } else if (event.type === "session.updated" && event.patch.modelId) {
      this.modelIdBySession[event.sessionId] = event.patch.modelId;
    }
  }

  private async replayAll(): Promise<void> {
    const cursors = [...this.cursorBySession.entries()];
    await Promise.all(
      cursors.map(async ([sessionId, cursor]) => {
        const query = new URLSearchParams({
          actorGeneration: String(cursor.actorGeneration),
          sequence: String(cursor.sequence),
        });
        const response = await fetch(
          `/api/v1/sessions/${encodeURIComponent(sessionId)}/replay?${query}`,
          {
            headers: { Accept: "application/json" },
            credentials: "same-origin",
          },
        );
        if (!response.ok) {
          throw new Error(`Replay failed with ${response.status}`);
        }
        const replay = projectReplayResponse(await response.json(), {
          summary: this.summaries.get(sessionId),
          models: this.models,
        });
        if (replay.type === "gap") {
          this.rememberSnapshot(replay.snapshot);
          this.dispatch({
            type: "session.snapshot",
            sessionId: replay.snapshot.sessionId,
            actorGeneration: replay.snapshot.actorGeneration,
            sequence: replay.snapshot.sequence,
            snapshot: replay.snapshot,
          });
          return;
        }
        for (const event of replay.events) {
          this.rememberEvent(event);
          this.dispatch(event);
        }
        if (replay.events.length === 0) {
          this.actorGenerationBySession[sessionId] = replay.actorGeneration;
          this.cursorBySession.set(sessionId, {
            actorGeneration: replay.actorGeneration,
            sequence: replay.sequence,
          });
        }
      }),
    );
  }
}

const loopbackDeviceStorageKey = "ygg:loopback-device-id";
const validDeviceId = /^[A-Za-z0-9_.:-]{1,128}$/;
let volatileLoopbackDeviceId: string | undefined;

export type TransportMode = "fixture" | "live";

export function transportModeFromSearch(search: string): TransportMode {
  return new URLSearchParams(search).get("transport") === "fixture"
    ? "fixture"
    : "live";
}

export function resolveClientDeviceId(): string | undefined {
  const injected =
    document
      .querySelector<HTMLMetaElement>('meta[name="ygg-device-id"]')
      ?.content.trim() ||
    document.documentElement.dataset.yggDeviceId?.trim();
  if (injected && validDeviceId.test(injected)) return injected;

  const host = window.location.hostname;
  if (host !== "localhost" && host !== "127.0.0.1" && host !== "::1") {
    return undefined;
  }
  try {
    const stored = window.localStorage.getItem(loopbackDeviceStorageKey);
    if (stored && validDeviceId.test(stored)) return stored;
  } catch {
    // A stable ID for this page lifetime is still better than an empty ID.
  }
  const generated =
    volatileLoopbackDeviceId ?? `browser-${crypto.randomUUID()}`;
  volatileLoopbackDeviceId = generated;
  try {
    window.localStorage.setItem(loopbackDeviceStorageKey, generated);
  } catch {
    // Storage can be unavailable in a hardened browser.
  }
  return generated;
}

export function createTransport(
  mode = transportModeFromSearch(window.location.search),
): YggTransport {
  if (mode === "fixture") {
    return new FixtureTransport();
  }
  return new HttpTransport(resolveClientDeviceId());
}
