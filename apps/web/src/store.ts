import { useSyncExternalStore } from "react";
import { rejectedCommandError } from "./command-error";
import type {
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  DocumentReference,
  HostEvent,
  HostBootstrap,
  ProjectCatalog,
  RepositoryContextSnapshot,
  ReasoningEffort,
  SessionEvent,
  SessionSnapshot,
  SessionSummary,
  TranscriptSearchRequest,
  TranscriptSearchResult,
  TrustedFileCatalog,
  TrustedFileEntry,
  TrustedFileRead,
  TrustedFileSearchResult,
} from "./protocol";
import {
  primeSessionItemIndex,
  reduceSessionEvent,
  reduceSessionEvents,
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
  projectCatalog: ProjectCatalog | null;
  selectedSessionId: string | null;
  sessions: Record<string, SessionSnapshot>;
}

const initialState: YggState = {
  ready: false,
  connecting: true,
  connection: "connecting",
  error: null,
  bootstrap: null,
  projectCatalog: null,
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
    if (
      summary.title !== snapshot.title ||
      summary.status !== snapshot.status ||
      summary.modelId !== snapshot.modelId
    ) {
      return {
        ...summary,
        title: snapshot.title,
        status: snapshot.status,
        modelId: snapshot.modelId,
        updatedAt: new Date().toISOString(),
      };
    }
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

    for (let eventIndex = 0; eventIndex < events.length; eventIndex += 1) {
      const event = events[eventIndex]!;
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
      const reducedEvents: SessionEvent[] = [event];
      if (event.type === "item.delta") {
        while (eventIndex + 1 < events.length) {
          const candidate = events[eventIndex + 1]!;
          if (
            candidate.type !== "item.delta" ||
            candidate.sessionId !== event.sessionId ||
            candidate.itemId !== event.itemId ||
            candidate.field !== event.field
          ) {
            break;
          }
          reducedEvents.push(candidate);
          eventIndex += 1;
        }
      }

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
          primeSessionItemIndex(event.snapshot);
          updated = event.snapshot;
        }
      } else if (current) {
        try {
          updated =
            reducedEvents.length === 1
              ? reduceSessionEvent(current, event)
              : reduceSessionEvents(current, reducedEvents);
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

      const bootstrap = next.bootstrap;
      let summaryChanged = false;
      const summaries = bootstrap?.sessions.map((summary) => {
        if (summary.id !== event.sessionId) return summary;
        const candidate = updateSummary(
          summary,
          event,
          updated,
          event.sessionId === next.selectedSessionId,
        );
        if (candidate !== summary) summaryChanged = true;
        return candidate;
      });
      const sessionChanged = Boolean(updated && updated !== current);
      if (summaryChanged || sessionChanged) {
        next = {
          ...next,
          bootstrap:
            bootstrap && summaries && summaryChanged
              ? { ...bootstrap, sessions: summaries }
              : bootstrap,
          sessions:
            updated && sessionChanged
              ? { ...next.sessions, [event.sessionId]: updated }
              : next.sessions,
        };
        changed = true;
      }
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
          primeSessionItemIndex(snapshot);
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
    this.publish({
      ...this.state,
      connecting: true,
      error: null,
    });
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
      const projectCatalog = await this.transport.getProjectCatalog();
      const hasRunnableProject = projectCatalog.projects.some(
        (project) =>
          project.trusted && project.available && !project.archived,
      );
      if (!hasRunnableProject) {
        this.publish({
          ready: true,
          connecting: false,
          connection: this.state.connection,
          error: null,
          bootstrap: null,
          projectCatalog,
          selectedSessionId: null,
          sessions: {},
        });
        return;
      }
      this.publish({
        ...this.state,
        projectCatalog,
      });
      const routedProjectId = this.state.bootstrap?.sessions.find(
        (summary) => summary.id === routedSessionId,
      )?.projectId;
      const routedProjectRunnable =
        routedProjectId === undefined ||
        projectCatalog.projects.some(
          (project) =>
            project.id === routedProjectId &&
            project.trusted &&
            project.available &&
            !project.archived,
        );
      const hostBootstrap = await this.transport.connect(
        routedProjectRunnable ? routedSessionId ?? undefined : undefined,
      );
      const bootstrap = hostBootstrap;
      const selected = await this.transport.getSession(
        bootstrap.selectedSessionId,
      );
      primeSessionItemIndex(selected);
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
        projectCatalog,
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

  ingestDocument(file: File): Promise<DocumentReference> {
    const session = this.selectedSession;
    if (!session) return Promise.reject(new Error("No session is selected."));
    return this.transport.ingestDocument(session.sessionId, file);
  }

  listDocuments(): Promise<DocumentReference[]> {
    const session = this.selectedSession;
    if (!session) return Promise.resolve([]);
    return this.transport.listDocuments(session.sessionId);
  }

  getTrustedFiles(projectId: string): Promise<TrustedFileCatalog> {
    return this.transport.getTrustedFiles(projectId);
  }

  getRepositoryContext(projectId: string): Promise<RepositoryContextSnapshot> {
    return this.transport.getRepositoryContext(projectId);
  }

  searchTrustedFiles(
    projectId: string,
    query: string,
  ): Promise<TrustedFileSearchResult> {
    return this.transport.searchTrustedFiles(projectId, query);
  }

  readTrustedFile(
    projectId: string,
    entryId: string,
  ): Promise<TrustedFileRead> {
    return this.transport.readTrustedFile(projectId, entryId);
  }

  searchTranscripts(
    request: TranscriptSearchRequest,
  ): Promise<TranscriptSearchResult> {
    return this.transport.searchTranscripts(request);
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
      primeSessionItemIndex(snapshot);
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
    await this.installCreatedSession(ack.createdSessionId, bootstrap);
  }

  private async installCreatedSession(
    sessionId: string,
    bootstrap: HostBootstrap,
  ): Promise<void> {
    const snapshot = await this.transport.getSession(sessionId);
    const summary: SessionSummary = {
      id: snapshot.sessionId,
      projectId: snapshot.projectId,
      title: snapshot.title,
      preview: "Ready when you are",
      status: snapshot.status,
      updatedAt: snapshot.startedAt,
      pinned: false,
      archived: false,
      lifecycle: "active",
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
    idempotencyKey?: string,
    documents: DocumentReference[] = [],
    projectFiles: TrustedFileEntry[] = [],
  ): Promise<void> {
    const session = this.selectedSession;
    if (
      !session ||
      (!prompt.trim() &&
        attachments.length === 0 &&
        documents.length === 0 &&
        projectFiles.length === 0)
    )
      return;
    const ack = await this.sendCommand({
      id: idempotencyKey ?? commandId(),
      type,
      sessionId: session.sessionId,
      prompt: prompt.trim(),
      attachments,
      documentIds: documents.map((document) => document.id),
      projectFileIds: projectFiles.map((file) => file.id),
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this message.",
      );
    }
  }

  async steer(
    prompt: string,
    attachments: AttachmentRef[],
    idempotencyKey?: string,
    documents: DocumentReference[] = [],
    projectFiles: TrustedFileEntry[] = [],
  ): Promise<void> {
    await this.sendInput(
      "session.steer",
      prompt,
      attachments,
      idempotencyKey,
      documents,
      projectFiles,
    );
  }

  async followUp(
    prompt: string,
    attachments: AttachmentRef[],
    idempotencyKey?: string,
    documents: DocumentReference[] = [],
    projectFiles: TrustedFileEntry[] = [],
  ): Promise<void> {
    await this.sendInput(
      "session.followUp",
      prompt,
      attachments,
      idempotencyKey,
      documents,
      projectFiles,
    );
  }

  async submit(
    prompt: string,
    attachments: AttachmentRef[],
    activeDelivery: "steer" | "followUp" = "followUp",
    idempotencyKey?: string,
    documents: DocumentReference[] = [],
    projectFiles: TrustedFileEntry[] = [],
  ): Promise<void> {
    const session = this.selectedSession;
    if (
      !session ||
      (!prompt.trim() &&
        attachments.length === 0 &&
        documents.length === 0 &&
        projectFiles.length === 0)
    )
      return;
    const activeRun =
      session.activeRunId !== undefined ||
      session.status === "working" ||
      session.status === "needs_attention";
    if (!activeRun) {
      await this.sendInput(
        "session.submit",
        prompt,
        attachments,
        idempotencyKey,
        documents,
        projectFiles,
      );
    } else if (activeDelivery === "steer") {
      await this.steer(
        prompt,
        attachments,
        idempotencyKey,
        documents,
        projectFiles,
      );
    } else {
      await this.followUp(
        prompt,
        attachments,
        idempotencyKey,
        documents,
        projectFiles,
      );
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
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this branch checkout.",
      );
    }
  }

  async editUserTurn(
    sourceUserEntryId: string,
    prompt: string,
    attachments: AttachmentRef[] = [],
    documents: DocumentReference[] = [],
    projectFiles: TrustedFileEntry[] = [],
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session || !prompt.trim()) return;
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.editUserTurn",
      sessionId: session.sessionId,
      sourceUserEntryId,
      prompt: prompt.trim(),
      attachments,
      documentIds: documents.map((document) => document.id),
      projectFileIds: projectFiles.map((file) => file.id),
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this edited turn.",
      );
    }
  }

  async retryResponse(
    sourceAssistantEntryId: string,
    model?: { id: string; reasoning: ReasoningEffort },
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session) return;
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.retryResponse",
      sessionId: session.sessionId,
      sourceAssistantEntryId,
      modelId: model?.id,
      reasoning: model?.reasoning,
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this response retry.",
      );
    }
  }

  async forkConversation(entryId: string): Promise<void> {
    const session = this.selectedSession;
    const bootstrap = this.state.bootstrap;
    if (!session || !bootstrap) return;
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.forkConversation",
      sessionId: session.sessionId,
      entryId,
    });
    if (!ack.accepted || !ack.createdSessionId) {
      if (!ack.accepted) {
        throw rejectedCommandError(
          ack,
          "The ygg host rejected this conversation fork.",
        );
      }
      throw new Error("The ygg host did not identify the forked session.");
    }
    await this.installCreatedSession(ack.createdSessionId, bootstrap);
  }

  async setSessionLifecycle(
    sessionId: string,
    lifecycle: "active" | "archived" | "trash",
  ): Promise<void> {
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.setLifecycle",
      sessionId,
      lifecycle,
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this session lifecycle change.",
      );
    }
    await this.initialize();
  }

  async deleteSessionPermanently(
    sessionId: string,
    trashedAtMs: number,
    phrase: string,
  ): Promise<void> {
    const ack = await this.sendCommand({
      id: commandId(),
      type: "session.deletePermanently",
      sessionId,
      confirmation: { sessionId, trashedAtMs, phrase },
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected permanent deletion.",
      );
    }
    await this.initialize();
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

  private async refreshProjectCatalog(): Promise<ProjectCatalog> {
    const projectCatalog = await this.transport.getProjectCatalog();
    const bootstrap = this.state.bootstrap;
    this.publish({
      ...this.state,
      projectCatalog,
      bootstrap: bootstrap
        ? {
            ...bootstrap,
            catalogRevision: Math.max(
              bootstrap.catalogRevision,
              projectCatalog.catalogRevision,
            ),
            projects: projectCatalog.projects,
          }
        : null,
    });
    return projectCatalog;
  }

  private async mutateProject(command: ClientCommand): Promise<void> {
    const ack = await this.sendCommand(command);
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this project change.",
      );
    }
    await this.refreshProjectCatalog();
  }

  async renameProject(projectId: string, displayName: string): Promise<void> {
    const name = displayName.trim();
    if (!name) return;
    await this.mutateProject({
      id: commandId(),
      type: "project.rename",
      projectId,
      displayName: name,
    });
  }

  async setDefaultProject(projectId: string | null): Promise<void> {
    await this.mutateProject(
      projectId
        ? {
            id: commandId(),
            type: "project.setDefault",
            projectId,
          }
        : {
            id: commandId(),
            type: "project.clearDefault",
          },
    );
  }

  async setProjectTrust(projectId: string, trusted: boolean): Promise<void> {
    const selectedProjectId = this.selectedSession?.projectId;
    await this.mutateProject({
      id: commandId(),
      type: "project.setTrust",
      projectId,
      trusted,
    });
    if (!this.state.bootstrap || selectedProjectId === projectId) {
      await this.initialize();
    }
  }

  async archiveProject(projectId: string): Promise<void> {
    const selectedProjectId = this.selectedSession?.projectId;
    await this.mutateProject({
      id: commandId(),
      type: "project.archive",
      projectId,
    });
    if (!this.state.bootstrap || selectedProjectId === projectId) {
      await this.initialize();
    }
  }

  async importProjectCandidate(
    candidateId: string,
    displayName?: string,
  ): Promise<void> {
    await this.mutateProject({
      id: commandId(),
      type: "project.import",
      candidateId,
      displayName: displayName?.trim() || undefined,
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
