import { useSyncExternalStore } from "react";
import type {
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  HostBootstrap,
  ReasoningEffort,
  SessionEvent,
  SessionSnapshot,
  SessionSummary,
} from "./protocol";
import {
  reduceSessionEvent,
  SessionGenerationMismatchError,
  SessionSequenceGapError,
} from "./reducer";
import type { YggTransport } from "./transport";

export interface YggState {
  ready: boolean;
  connecting: boolean;
  error: string | null;
  bootstrap: HostBootstrap | null;
  selectedSessionId: string | null;
  sessions: Record<string, SessionSnapshot>;
}

const initialState: YggState = {
  ready: false,
  connecting: true,
  error: null,
  bootstrap: null,
  selectedSessionId: null,
  sessions: {},
};

const commandId = () => crypto.randomUUID();

export type SessionRouteMode = "push" | "replace" | "none";

export function sessionIdFromPathname(pathname: string): string | null {
  const match = /^\/session\/([^/]+)\/?$/.exec(pathname);
  if (!match?.[1]) return null;
  try {
    const sessionId = decodeURIComponent(match[1]);
    return sessionId.trim() && !sessionId.includes("/") ? sessionId : null;
  } catch {
    return null;
  }
}

function writeSessionRoute(
  sessionId: string,
  mode: Exclude<SessionRouteMode, "none">,
): void {
  const route = `/session/${encodeURIComponent(sessionId)}${window.location.search}`;
  if (mode === "replace") {
    window.history.replaceState(null, "", route);
  } else {
    window.history.pushState(null, "", route);
  }
}

function themeStorageKey(hostId: string): string {
  return `ygg:theme:${hostId}`;
}

function readLocalTheme(bootstrap: HostBootstrap): string | undefined {
  try {
    const stored = window.localStorage.getItem(themeStorageKey(bootstrap.host.id));
    return bootstrap.themes.some((theme) => theme.id === stored)
      ? stored ?? undefined
      : undefined;
  } catch {
    return undefined;
  }
}

function latestAssistant(snapshot: SessionSnapshot): string | undefined {
  return snapshot.items
    .filter((item) => item.kind === "assistant_message")
    .at(-1)?.content;
}

function updateSummary(
  summary: SessionSummary,
  event: SessionEvent,
  snapshot?: SessionSnapshot,
): SessionSummary {
  if (event.type === "session.snapshot") {
    return {
      ...summary,
      title: event.snapshot.title,
      status: event.snapshot.status,
      modelId: event.snapshot.modelId,
      preview: latestAssistant(event.snapshot) || summary.preview,
      updatedAt: new Date().toISOString(),
    };
  }
  if (event.type === "session.updated") {
    return {
      ...summary,
      title: event.patch.title ?? summary.title,
      status: event.patch.status ?? summary.status,
      modelId: event.patch.modelId ?? summary.modelId,
      updatedAt: new Date().toISOString(),
    };
  }
  if (
    event.type === "item.committed" &&
    event.item.kind === "assistant_message" &&
    event.item.content
  ) {
    return {
      ...summary,
      preview: event.item.content,
      updatedAt: new Date().toISOString(),
    };
  }
  if (snapshot) {
    return {
      ...summary,
      title: snapshot.title,
      status: snapshot.status,
      modelId: snapshot.modelId,
      updatedAt: new Date().toISOString(),
    };
  }
  return summary;
}

export class YggStore {
  private state: YggState = initialState;
  private listeners = new Set<() => void>();
  private unsubscribeTransport: (() => void) | null = null;
  private queuedEvents: SessionEvent[] = [];
  private animationFrame: number | null = null;
  private selectionGeneration = 0;
  private selectionAbort: AbortController | null = null;
  private resyncing = new Set<string>();
  private disposed = false;

  constructor(private readonly transport: YggTransport) {}

  getSnapshot = (): YggState => this.state;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  private publish(next: YggState): void {
    if (this.disposed) return;
    this.state = next;
    for (const listener of this.listeners) listener();
  }

  private queueEvent(event: SessionEvent): void {
    this.queuedEvents.push(event);
    if (this.animationFrame !== null) return;
    this.animationFrame = window.requestAnimationFrame(() => {
      this.animationFrame = null;
      this.flushEvents();
    });
  }

  private flushEvents(): void {
    const events = this.queuedEvents;
    this.queuedEvents = [];
    if (!events.length || !this.state.bootstrap) return;

    let next = this.state;
    let changed = false;

    for (const event of events) {
      const current = next.sessions[event.sessionId];
      let updated = current;

      if (event.type === "session.snapshot") {
        if (
          event.snapshot.sessionId === event.sessionId &&
          event.snapshot.sequence === event.sequence
        ) {
          updated = event.snapshot;
        }
      } else if (current) {
        try {
          updated = reduceSessionEvent(current, event);
        } catch (error) {
          if (
            error instanceof SessionSequenceGapError ||
            error instanceof SessionGenerationMismatchError
          ) {
            void this.resyncSession(event.sessionId);
            continue;
          }
          throw error;
        }
      }

      const summaries = next.bootstrap?.sessions.map((summary) =>
        summary.id === event.sessionId
          ? updateSummary(summary, event, updated)
          : summary,
      );
      next = {
        ...next,
        bootstrap:
          next.bootstrap && summaries
            ? { ...next.bootstrap, sessions: summaries }
            : next.bootstrap,
        sessions: updated
          ? { ...next.sessions, [event.sessionId]: updated }
          : next.sessions,
      };
      changed = true;
    }

    if (changed) this.publish(next);
  }

  private async resyncSession(sessionId: string): Promise<void> {
    if (this.resyncing.has(sessionId)) return;
    this.resyncing.add(sessionId);
    try {
      const snapshot = await this.transport.getSession(sessionId);
      const bootstrap = this.state.bootstrap;
      const summaries = bootstrap?.sessions.map((summary) =>
        summary.id === sessionId
          ? {
              ...summary,
              title: snapshot.title,
              status: snapshot.status,
              modelId: snapshot.modelId,
              preview: latestAssistant(snapshot) || summary.preview,
            }
          : summary,
      );
      this.publish({
        ...this.state,
        bootstrap:
          bootstrap && summaries ? { ...bootstrap, sessions: summaries } : bootstrap,
        sessions: { ...this.state.sessions, [sessionId]: snapshot },
      });
    } catch {
      // The transport reconnect path will replay or deliver a newer snapshot.
    } finally {
      this.resyncing.delete(sessionId);
    }
  }

  private async sendCommand(command: ClientCommand) {
    try {
      return await this.transport.send(command);
    } catch {
      // A retry preserves the command UUID so the host can deduplicate it.
      return this.transport.send(command);
    }
  }

  async initialize(): Promise<void> {
    this.disposed = false;
    this.unsubscribeTransport = this.transport.subscribe((event) => {
      this.queueEvent(event);
    });

    try {
      const routedSessionId = sessionIdFromPathname(window.location.pathname);
      const hostBootstrap = await this.transport.connect(
        routedSessionId ?? undefined,
      );
      const bootstrap: HostBootstrap = {
        ...hostBootstrap,
        selectedThemeId:
          readLocalTheme(hostBootstrap) ?? hostBootstrap.selectedThemeId,
      };
      const selected = await this.transport.getSession(
        bootstrap.selectedSessionId,
      );
      this.publish({
        ready: true,
        connecting: false,
        error: null,
        bootstrap,
        selectedSessionId: selected.sessionId,
        sessions: { [selected.sessionId]: selected },
      });
      writeSessionRoute(selected.sessionId, "replace");
    } catch (error) {
      this.publish({
        ...this.state,
        connecting: false,
        error:
          error instanceof Error ? error.message : "ygg could not connect.",
      });
    }
  }

  ingestAttachment(file: File): Promise<AttachmentRef> {
    return this.transport.ingestAttachment(file);
  }

  attachmentContentUrl(handle: string): string {
    return this.transport.attachmentContentUrl(handle);
  }

  async selectSession(
    sessionId: string,
    routeMode: SessionRouteMode = "push",
  ): Promise<void> {
    if (this.state.selectedSessionId === sessionId) {
      this.selectionGeneration += 1;
      this.selectionAbort?.abort();
      this.selectionAbort = null;
      if (routeMode !== "none") writeSessionRoute(sessionId, routeMode);
      return;
    }
    const generation = ++this.selectionGeneration;
    this.selectionAbort?.abort();
    const controller = new AbortController();
    this.selectionAbort = controller;

    try {
      const snapshot =
        this.state.sessions[sessionId] ??
        (await this.transport.getSession(sessionId, controller.signal));
      if (generation !== this.selectionGeneration || controller.signal.aborted) {
        return;
      }
      this.publish({
        ...this.state,
        selectedSessionId: sessionId,
        sessions: { ...this.state.sessions, [sessionId]: snapshot },
      });
      if (routeMode !== "none") writeSessionRoute(sessionId, routeMode);
      this.selectionAbort = null;
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") return;
      throw error;
    }
  }

  async createSession(): Promise<void> {
    const bootstrap = this.state.bootstrap;
    if (!bootstrap) return;
    const selected = this.selectedSession;
    const command: ClientCommand = {
      id: commandId(),
      type: "session.create",
      projectId: selected?.projectId ?? bootstrap.projects[0]?.id ?? "default",
      modelId: selected?.modelId ?? bootstrap.models[0]?.id ?? "default",
      reasoning:
        selected?.reasoning ??
        bootstrap.models[0]?.defaultReasoning ??
        bootstrap.models[0]?.reasoning[0] ??
        "off",
      authority: selected?.authority ?? "fullAccess",
    };
    const ack = await this.sendCommand(command);
    if (!ack.accepted || !ack.createdSessionId) return;
    const snapshot = await this.transport.getSession(ack.createdSessionId);
    const summary: SessionSummary = {
      id: snapshot.sessionId,
      projectId: snapshot.projectId,
      title: snapshot.title,
      preview: "Ready when you are",
      status: snapshot.status,
      updatedAt: snapshot.startedAt,
      pinned: false,
      archived: false,
      unread: false,
      modelId: snapshot.modelId,
      attentionCount: 0,
    };
    this.publish({
      ...this.state,
      bootstrap: {
        ...bootstrap,
        selectedSessionId: snapshot.sessionId,
        sessions: [summary, ...bootstrap.sessions],
      },
      selectedSessionId: snapshot.sessionId,
      sessions: { ...this.state.sessions, [snapshot.sessionId]: snapshot },
    });
    writeSessionRoute(snapshot.sessionId, "push");
  }

  private async sendInput(
    type: "session.submit" | "session.steer" | "session.followUp",
    prompt: string,
    attachments: AttachmentRef[],
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session || (!prompt.trim() && attachments.length === 0)) return;
    const ack = await this.sendCommand({
      id: commandId(),
      type,
      sessionId: session.sessionId,
      prompt: prompt.trim(),
      attachments,
    });
    if (!ack.accepted) {
      throw new Error(ack.error ?? "The ygg host rejected this message.");
    }
  }

  async steer(prompt: string, attachments: AttachmentRef[]): Promise<void> {
    await this.sendInput("session.steer", prompt, attachments);
  }

  async followUp(prompt: string, attachments: AttachmentRef[]): Promise<void> {
    await this.sendInput("session.followUp", prompt, attachments);
  }

  async submit(
    prompt: string,
    attachments: AttachmentRef[],
    activeDelivery: "steer" | "followUp" = "followUp",
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session || (!prompt.trim() && attachments.length === 0)) return;
    if (session.status !== "working") {
      await this.sendInput("session.submit", prompt, attachments);
    } else if (activeDelivery === "steer") {
      await this.steer(prompt, attachments);
    } else {
      await this.followUp(prompt, attachments);
    }
  }

  async interrupt(): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    await this.sendCommand({
      id: commandId(),
      type: "session.interrupt",
      sessionId: session.sessionId,
    });
  }

  async configure(
    patch: {
      modelId?: string;
      reasoning?: ReasoningEffort;
      authority?: AuthorityProfile;
    },
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    await this.sendCommand({
      id: commandId(),
      type: "session.configure",
      sessionId: session.sessionId,
      ...patch,
    });
  }

  async rename(title: string): Promise<void> {
    const session = this.selectedSession;
    if (!session || !title.trim()) return;
    await this.sendCommand({
      id: commandId(),
      type: "session.rename",
      sessionId: session.sessionId,
      title: title.trim(),
    });
  }

  async pin(pinned: boolean): Promise<void> {
    const session = this.selectedSession;
    const bootstrap = this.state.bootstrap;
    if (!session || !bootstrap) return;
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.pin",
      sessionId: session.sessionId,
      pinned,
    });
    if (!ack.accepted) return;
    this.publish({
      ...this.state,
      bootstrap: {
        ...bootstrap,
        sessions: bootstrap.sessions.map((summary) =>
          summary.id === session.sessionId ? { ...summary, pinned } : summary,
        ),
      },
    });
  }

  async archive(archived: boolean): Promise<boolean> {
    const session = this.selectedSession;
    const bootstrap = this.state.bootstrap;
    if (!session || !bootstrap) return false;
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.archive",
      sessionId: session.sessionId,
      archived,
    });
    if (!ack.accepted) return false;
    this.publish({
      ...this.state,
      bootstrap: {
        ...bootstrap,
        sessions: bootstrap.sessions.map((summary) =>
          summary.id === session.sessionId ? { ...summary, archived } : summary,
        ),
      },
    });
    return true;
  }

  async resolveApproval(
    requestId: string,
    decision: "allowed_once" | "allowed_session" | "denied",
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    await this.sendCommand({
      id: commandId(),
      type: "approval.resolve",
      sessionId: session.sessionId,
      requestId,
      decision,
    });
  }

  async selectTheme(themeId: string): Promise<void> {
    const bootstrap = this.state.bootstrap;
    if (!bootstrap || bootstrap.selectedThemeId === themeId) return;
    if (!bootstrap.themes.some((theme) => theme.id === themeId)) return;
    try {
      window.localStorage.setItem(themeStorageKey(bootstrap.host.id), themeId);
    } catch {
      // Local presentation still changes for this page lifetime.
    }
    this.publish({
      ...this.state,
      bootstrap: { ...bootstrap, selectedThemeId: themeId },
    });
  }

  get selectedSession(): SessionSnapshot | null {
    const id = this.state.selectedSessionId;
    return id ? (this.state.sessions[id] ?? null) : null;
  }

  dispose(): void {
    this.disposed = true;
    this.selectionGeneration += 1;
    this.selectionAbort?.abort();
    if (this.animationFrame !== null) {
      window.cancelAnimationFrame(this.animationFrame);
    }
    this.animationFrame = null;
    this.queuedEvents = [];
    this.unsubscribeTransport?.();
    this.transport.close();
    this.listeners.clear();
  }
}

export function useYggStore(store: YggStore): YggState {
  return useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getSnapshot,
  );
}
