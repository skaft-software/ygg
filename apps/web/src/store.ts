import { useSyncExternalStore } from "react";
import type {
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  HostEvent,
  HostBootstrap,
  ReasoningEffort,
  SessionEvent,
  SessionSnapshot,
  SessionSummary,
} from "./protocol";
import {
  reduceSessionEvent,
  SessionBranchGraphError,
  SessionGenerationMismatchError,
  SessionProjectionReplacementRequiredError,
  SessionSequenceGapError,
} from "./reducer";
import {
  deriveSessionTitle,
  isUntitledSession,
} from "./session-title";
import type {
  TransportConnectionState,
  YggTransport,
} from "./transport";

export interface YggState {
  ready: boolean;
  connecting: boolean;
  connection: TransportConnectionState;
  error: string | null;
  bootstrap: HostBootstrap | null;
  selectedSessionId: string | null;
  sessions: Record<string, SessionSnapshot>;
}

const initialState: YggState = {
  ready: false,
  connecting: true,
  connection: "connecting",
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
  selected = false,
): SessionSummary {
  if (event.type === "session.snapshot") {
    return {
      ...summary,
      title: event.snapshot.title,
      status: event.snapshot.status,
      modelId: event.snapshot.modelId,
      preview: latestAssistant(event.snapshot) || summary.preview,
      updatedAt: new Date().toISOString(),
      unread:
        selected ||
        event.snapshot.status === "working" ||
        event.snapshot.status === "needs_attention" ||
        event.snapshot.status === "failed"
          ? false
          : event.snapshot.status === "done"
            ? true
            : summary.unread,
      attentionCount:
        event.snapshot.status === "needs_attention" ||
        event.snapshot.status === "failed"
          ? 1
          : 0,
    };
  }
  if (event.type === "session.updated") {
    const status = event.patch.status ?? summary.status;
    return {
      ...summary,
      title: event.patch.title ?? summary.title,
      status,
      modelId: event.patch.modelId ?? summary.modelId,
      updatedAt: new Date().toISOString(),
      unread:
        selected ||
        status === "working" ||
        status === "needs_attention" ||
        status === "failed"
          ? false
          : status === "done"
            ? true
            : summary.unread,
      attentionCount:
        status === "needs_attention" || status === "failed" ? 1 : 0,
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
  private unsubscribeConnection: (() => void) | null = null;
  private queuedEvents: HostEvent[] = [];
  private animationFrame: number | null = null;
  private selectionGeneration = 0;
  private selectionAbort: AbortController | null = null;
  private resyncing = new Set<string>();
  private deferredDuringResync = new Map<string, SessionEvent[]>();
  private createSessionTail: Promise<void> = Promise.resolve();
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

  private queueEvent(event: HostEvent): void {
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
      if (event.type === "catalog.summary") {
        const bootstrap = next.bootstrap;
        if (
          !bootstrap ||
          event.catalogRevision < bootstrap.catalogRevision
        ) {
          continue;
        }
        const summary =
          event.summary.id === next.selectedSessionId
            ? { ...event.summary, unread: false }
            : event.summary;
        const exists = bootstrap.sessions.some(
          (candidate) => candidate.id === summary.id,
        );
        next = {
          ...next,
          bootstrap: {
            ...bootstrap,
            catalogRevision: event.catalogRevision,
            sessions: exists
              ? bootstrap.sessions.map((candidate) =>
                  candidate.id === summary.id ? summary : candidate,
                )
              : [summary, ...bootstrap.sessions],
          },
        };
        changed = true;
        continue;
      }
      if (this.resyncing.has(event.sessionId)) {
        const deferred = this.deferredDuringResync.get(event.sessionId) ?? [];
        deferred.push(event);
        this.deferredDuringResync.set(event.sessionId, deferred);
        continue;
      }
      const current = next.sessions[event.sessionId];
      let updated = current;

      if (event.type === "session.projectionReplaced") {
        const incomingGeneration =
          event.actorGeneration ?? current?.actorGeneration;
        const stale =
          current !== undefined &&
          incomingGeneration !== undefined &&
          (incomingGeneration < current.actorGeneration ||
            (incomingGeneration === current.actorGeneration &&
              event.sequence <= current.sequence));
        if (!stale) {
          void this.resyncSession(event.sessionId, {
            actorGeneration: incomingGeneration ?? 0,
            sequence: event.sequence,
          });
        }
        continue;
      }
      if (event.type === "session.snapshot" && !current) {
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
            error instanceof SessionGenerationMismatchError ||
            error instanceof SessionBranchGraphError ||
            error instanceof SessionProjectionReplacementRequiredError
          ) {
            void this.resyncSession(event.sessionId);
            continue;
          }
          throw error;
        }
      }
      if (
        updated &&
        isUntitledSession(updated.title) &&
        (event.type === "item.started" || event.type === "item.committed") &&
        event.item.kind === "user_message"
      ) {
        updated = {
          ...updated,
          title: deriveSessionTitle(
            event.item.content,
            event.item.attachments?.at(0)?.name,
          ),
        };
      }

      const summaries = next.bootstrap?.sessions.map((summary) =>
        summary.id === event.sessionId
          ? updateSummary(
              summary,
              event,
              updated,
              event.sessionId === next.selectedSessionId,
            )
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

  private async resyncSession(
    sessionId: string,
    required?: { actorGeneration: number; sequence: number },
  ): Promise<void> {
    if (this.resyncing.has(sessionId)) return;
    this.resyncing.add(sessionId);
    let installed = false;
    let retryDelay = 50;
    try {
      while (!this.disposed && !installed) {
        try {
          const snapshot = await this.transport.getSession(sessionId);
          const current = this.state.sessions[sessionId];
          const predates = (
            cursor: { actorGeneration: number; sequence: number },
            minimum: { actorGeneration: number; sequence: number },
          ) =>
            cursor.actorGeneration < minimum.actorGeneration ||
            (cursor.actorGeneration === minimum.actorGeneration &&
              cursor.sequence < minimum.sequence);
          if (
            (required &&
              predates(
                {
                  actorGeneration: snapshot.actorGeneration,
                  sequence: snapshot.sequence,
                },
                required,
              )) ||
            (current &&
              predates(
                {
                  actorGeneration: snapshot.actorGeneration,
                  sequence: snapshot.sequence,
                },
                {
                  actorGeneration: current.actorGeneration,
                  sequence: current.sequence,
                },
              ))
          ) {
            throw new Error("Session resync returned a stale projection.");
          }
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
              bootstrap && summaries
                ? { ...bootstrap, sessions: summaries }
                : bootstrap,
            sessions: { ...this.state.sessions, [sessionId]: snapshot },
          });
          installed = true;
        } catch {
          if (this.disposed) break;
          await new Promise<void>((resolve) => {
            window.setTimeout(resolve, retryDelay);
          });
          retryDelay = Math.min(2_000, retryDelay * 2);
        }
      }
    } finally {
      this.resyncing.delete(sessionId);
      const deferred = this.deferredDuringResync.get(sessionId) ?? [];
      this.deferredDuringResync.delete(sessionId);
      if (installed) {
        for (const event of deferred) this.queueEvent(event);
      }
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
    this.unsubscribeTransport?.();
    this.unsubscribeConnection?.();
    this.unsubscribeTransport = this.transport.subscribe((event) => {
      this.queueEvent(event);
    });
    this.unsubscribeConnection =
      this.transport.subscribeConnection?.((connection) => {
        this.publish({ ...this.state, connection });
      }) ?? null;

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
      const summaries = bootstrap.sessions.map((summary) =>
        summary.id === selected.sessionId
          ? { ...summary, title: selected.title }
          : summary,
      );
      this.publish({
        ready: true,
        connecting: false,
        connection: this.state.connection,
        error: null,
        bootstrap: { ...bootstrap, sessions: summaries },
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

  resourceContentUrl(sessionId: string, handle: string): string {
    return this.transport.resourceContentUrl(sessionId, handle);
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
        bootstrap: this.state.bootstrap
          ? {
              ...this.state.bootstrap,
              sessions: this.state.bootstrap.sessions.map((summary) =>
                summary.id === sessionId
                  ? { ...summary, title: snapshot.title, unread: false }
                  : summary,
              ),
            }
          : this.state.bootstrap,
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

  createSession(): Promise<void> {
    const operation = this.createSessionTail.then(() =>
      this.createSessionNow(),
    );
    this.createSessionTail = operation.catch(() => {});
    return operation;
  }

  private async createSessionNow(): Promise<void> {
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
    const currentBootstrap = this.state.bootstrap ?? bootstrap;
    this.publish({
      ...this.state,
      bootstrap: {
        ...currentBootstrap,
        selectedSessionId: snapshot.sessionId,
        sessions: [
          summary,
          ...currentBootstrap.sessions.filter(
            (candidate) => candidate.id !== summary.id,
          ),
        ],
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
    const activeRun =
      session.activeRunId !== undefined ||
      session.status === "working" ||
      session.status === "needs_attention";
    if (!activeRun) {
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

  async checkoutBranch(entryId: string): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    if (
      session.activeRunId !== undefined ||
      !["idle", "done", "failed", "stopped"].includes(session.status)
    ) {
      throw new Error(
        "A session branch can only be checked out after current work finishes.",
      );
    }
    const target = session.branches.entries.find(
      (entry) => entry.entryId === entryId && entry.checkoutable,
    );
    if (!target) {
      throw new Error("That session checkpoint is not available for checkout.");
    }
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.checkout",
      sessionId: session.sessionId,
      entryId,
    });
    if (!ack.accepted) {
      throw new Error(ack.error ?? "The ygg host rejected this branch checkout.");
    }
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

  async resolveUserInput(
    requestId: string,
    answer:
      | { type: "text"; text: string }
      | { type: "choice"; choice: string },
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    await this.sendCommand({
      id: commandId(),
      type: "userInput.resolve",
      sessionId: session.sessionId,
      requestId,
      answer,
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
    this.deferredDuringResync.clear();
    this.unsubscribeTransport?.();
    this.unsubscribeConnection?.();
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
