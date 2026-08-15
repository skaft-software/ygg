import { useSyncExternalStore } from "react";
import { rejectedCommandError } from "./command-error";
import type {
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  CommandDiscovery,
  DocumentReference,
  GoalState,
  GoalMutation,
  HostEvent,
  HostBootstrap,
  LifetimeUsage,
  ProjectCatalog,
  ProjectFileRead,
  ProjectFileSearchResult,
  ProjectFileTree,
  ProjectFileWrite,
  ProjectFileWriteRequest,
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
  UsageActivity,
  UsagePeriod,
  UsageStats,
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
import { isUntitledSession } from "./session-title";
import type { TransportConnectionState, YggTransport } from "./transport";

export interface YggState {
  ready: boolean;
  connecting: boolean;
  connection: TransportConnectionState;
  error: string | null;
  selectionError: {
    sessionId: string;
    message: string;
    routeMode: SessionRouteMode;
  } | null;
  bootstrap: HostBootstrap | null;
  projectCatalog: ProjectCatalog | null;
  selectedSessionId: string | null;
  goal: GoalState | null;
  sessions: Record<string, SessionSnapshot>;
}

const initialState: YggState = {
  ready: false,
  connecting: true,
  connection: "connecting",
  error: null,
  selectionError: null,
  bootstrap: null,
  projectCatalog: null,
  selectedSessionId: null,
  goal: null,
  sessions: {},
};

const commandId = () => crypto.randomUUID();

function acceptsGoalProjection(
  current: GoalState | null,
  candidate: GoalState | null,
  currentRevision: number,
  candidateRevision: number,
): boolean {
  if (candidateRevision < currentRevision) return false;
  return (
    candidate === null ||
    current === null ||
    candidate.revision >= current.revision
  );
}

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

function stableSessionTitle(current: string, incoming: string): string {
  return isUntitledSession(incoming) && !isUntitledSession(current)
    ? current
    : incoming;
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
      title: stableSessionTitle(summary.title, event.snapshot.title),
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
  if (event.type === "session.pullRequestChanged") {
    // Session events advance the replay cursor, but catalog summaries own PR
    // evidence. Keeping that ownership split prevents a delayed actor event
    // from regressing a newer host/inventory catalog projection.
    return summary;
  }
  if (event.type === "session.updated") {
    const status = event.patch.status ?? summary.status;
    return {
      ...summary,
      title:
        event.patch.title === undefined
          ? summary.title
          : stableSessionTitle(summary.title, event.patch.title),
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
    const title = stableSessionTitle(summary.title, snapshot.title);
    if (
      summary.title !== title ||
      summary.status !== snapshot.status ||
      summary.modelId !== snapshot.modelId
    ) {
      return {
        ...summary,
        title,
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
  private initializationGeneration = 0;
  private goalRevision = 0;
  private disposed = false;

  constructor(private readonly transport: YggTransport) {}

  getSnapshot = (): YggState => this.state;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  private isCurrentInitialization(generation: number): boolean {
    return !this.disposed && generation === this.initializationGeneration;
  }

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
    let goalRevision = this.goalRevision;
    let changed = false;

    for (let eventIndex = 0; eventIndex < events.length; eventIndex += 1) {
      const event = events[eventIndex]!;
      if (event.type === "catalog.summary") {
        const bootstrap = next.bootstrap;
        if (!bootstrap || event.catalogRevision < bootstrap.catalogRevision) {
          continue;
        }
        const existingSummary = bootstrap.sessions.find(
          (candidate) => candidate.id === event.summary.id,
        );
        const currentSession = next.sessions[event.summary.id];
        const priorTitle =
          currentSession && !isUntitledSession(currentSession.title)
            ? currentSession.title
            : (existingSummary?.title ?? currentSession?.title);
        const mergedSummary = {
          ...event.summary,
          title: priorTitle
            ? stableSessionTitle(priorTitle, event.summary.title)
            : event.summary.title,
        };
        const summary =
          mergedSummary.id === next.selectedSessionId
            ? { ...mergedSummary, unread: false }
            : mergedSummary;
        const exists = Boolean(existingSummary);
        const sessionTitle = currentSession
          ? stableSessionTitle(currentSession.title, summary.title)
          : undefined;
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
          sessions:
            currentSession &&
            sessionTitle !== undefined &&
            sessionTitle !== currentSession.title
              ? {
                  ...next.sessions,
                  [summary.id]: { ...currentSession, title: sessionTitle },
                }
              : next.sessions,
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
      if (updated && current) {
        const title = stableSessionTitle(current.title, updated.title);
        if (title !== updated.title) updated = { ...updated, title };
      }

      // An unloaded session has no actor-generation/sequence cursor with which
      // to reject replayed PR events; its catalog summary stays authoritative.
      const summaryEventAccepted = current
        ? updated !== current
        : event.type === "session.snapshot"
          ? updated !== undefined
          : event.type !== "session.pullRequestChanged";
      const bootstrap = next.bootstrap;
      let summaryChanged = false;
      const summaries = bootstrap?.sessions.map((summary) => {
        if (summary.id !== event.sessionId || !summaryEventAccepted) return summary;
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
      const goalEventAccepted =
        event.type === "session.goalChanged" &&
        event.sessionId === next.selectedSessionId &&
        acceptsGoalProjection(
          next.goal,
          event.goal,
          goalRevision,
          event.revision,
        );
      const goalChanged =
        goalEventAccepted &&
        current !== undefined &&
        updated !== current;
      if (goalChanged) {
        goalRevision = Math.max(
          goalRevision,
          event.revision,
          event.goal?.revision ?? 0,
        );
      }
      if (summaryChanged || sessionChanged || goalChanged) {
        next = {
          ...next,
          bootstrap:
            bootstrap && summaries && summaryChanged
              ? { ...bootstrap, sessions: summaries }
              : bootstrap,
          goal: goalChanged ? event.goal : next.goal,
          sessions:
            updated && sessionChanged
              ? { ...next.sessions, [event.sessionId]: updated }
              : next.sessions,
        };
        changed = true;
      }
    }

    if (changed) {
      this.goalRevision = goalRevision;
      this.publish(next);
    }
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
          const summary = bootstrap?.sessions.find(
            (candidate) => candidate.id === sessionId,
          );
          const priorTitle =
            current && !isUntitledSession(current.title)
              ? current.title
              : (summary?.title ?? current?.title);
          const replacement = priorTitle
            ? {
                ...snapshot,
                title: stableSessionTitle(priorTitle, snapshot.title),
              }
            : snapshot;
          primeSessionItemIndex(replacement);
          const summaries = bootstrap?.sessions.map((summary) =>
            summary.id === sessionId
              ? {
                  ...summary,
                  title: stableSessionTitle(summary.title, replacement.title),
                  status: replacement.status,
                  modelId: replacement.modelId,
                  preview: latestAssistant(replacement) || summary.preview,
                }
              : summary,
          );
          this.publish({
            ...this.state,
            bootstrap:
              bootstrap && summaries
                ? { ...bootstrap, sessions: summaries }
                : bootstrap,
            sessions: { ...this.state.sessions, [sessionId]: replacement },
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
    const generation = ++this.initializationGeneration;
    this.disposed = false;
    if (this.animationFrame !== null) {
      window.cancelAnimationFrame(this.animationFrame);
      this.animationFrame = null;
    }
    this.queuedEvents = [];
    this.publish({
      ...this.state,
      connecting: true,
      error: null,
      selectionError: null,
    });
    this.unsubscribeTransport?.();
    this.unsubscribeConnection?.();
    this.unsubscribeTransport = this.transport.subscribe((event) => {
      if (this.isCurrentInitialization(generation)) this.queueEvent(event);
    });
    this.unsubscribeConnection =
      this.transport.subscribeConnection?.((connection) => {
        if (this.isCurrentInitialization(generation)) {
          this.publish({ ...this.state, connection });
        }
      }) ?? null;

    try {
      const routedSessionId = sessionIdFromPathname(window.location.pathname);
      const projectCatalog = await this.transport.getProjectCatalog();
      if (!this.isCurrentInitialization(generation)) return;
      const hasRunnableProject = projectCatalog.projects.some(
        (project) => project.trusted && project.available && !project.archived,
      );
      if (!hasRunnableProject) {
        this.goalRevision = 0;
        this.publish({
          ready: true,
          connecting: false,
          connection: this.state.connection,
          error: null,
          selectionError: null,
          bootstrap: null,
          projectCatalog,
          selectedSessionId: null,
          goal: null,
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
      const inventoryOnly =
        routedSessionId === null && window.location.pathname === "/overview";
      const bootstrap = await this.transport.connect(
        routedProjectRunnable ? routedSessionId ?? undefined : undefined,
        inventoryOnly,
      );
      if (!this.isCurrentInitialization(generation)) return;
      if (bootstrap.selectedSessionId === null) {
        this.goalRevision = 0;
        this.publish({
          ready: true,
          connecting: false,
          connection: this.state.connection,
          error: null,
          selectionError: null,
          bootstrap,
          projectCatalog,
          selectedSessionId: null,
          goal: null,
          sessions: {},
        });
        return;
      }
      const selectedSessionId = bootstrap.selectedSessionId;
      const [selected, goalResponse] = await Promise.all([
        this.transport.getSession(selectedSessionId),
        this.transport.getGoal(selectedSessionId),
      ]);
      if (!this.isCurrentInitialization(generation)) return;
      const goal = goalResponse.goal;
      const selectedSummaryTitle = bootstrap.sessions.find(
        (summary) => summary.id === selected.sessionId,
      )?.title;
      const installedSelected = selectedSummaryTitle
        ? {
            ...selected,
            title: stableSessionTitle(selectedSummaryTitle, selected.title),
          }
        : selected;
      primeSessionItemIndex(installedSelected);

      const summaries = bootstrap.sessions.map((summary) =>
        summary.id === installedSelected.sessionId
          ? {
              ...summary,
              title: stableSessionTitle(summary.title, installedSelected.title),
            }
          : summary,
      );
      if (!this.isCurrentInitialization(generation)) return;
      this.goalRevision = goalResponse.revision;
      this.publish({
        ready: true,
        connecting: false,
        connection: this.state.connection,
        error: null,
        selectionError: null,
        bootstrap: { ...bootstrap, sessions: summaries },
        projectCatalog,
        selectedSessionId: installedSelected.sessionId,
        goal,
        sessions: { [installedSelected.sessionId]: installedSelected },
      });
      if (
        this.isCurrentInitialization(generation) &&
        window.location.pathname !== "/overview"
      ) {
        writeSessionRoute(installedSelected.sessionId, "replace");
      }
    } catch (error) {
      if (!this.isCurrentInitialization(generation)) return;
      this.publish({
        ...this.state,
        connecting: false,
        error: error instanceof Error ? error.message : "ygg could not connect.",
      });
    }
  }

  ingestAttachment(file: File): Promise<AttachmentRef> {
    return this.transport.ingestAttachment(file);
  }

  ingestDocument(file: File): Promise<DocumentReference> {
    const session = this.selectedSession;
    if (!session) return Promise.reject(new Error("No task is selected."));
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

  getCommandDiscovery(): Promise<CommandDiscovery> {
    const session = this.selectedSession;
    if (!session) return Promise.reject(new Error("No task is selected."));
    return this.transport.getCommandDiscovery(session.sessionId);
  }

  async invokeSlashCommand(
    invocation: string,
    idempotencyKey: string = commandId(),
  ): Promise<void> {
    const session = this.selectedSession;
    if (!session) throw new Error("No task is selected.");
    const ack = await this.sendCommand({
      id: idempotencyKey,
      type: "session.invokeSlashCommand",
      sessionId: session.sessionId,
      invocation,
    });
    if (!ack.accepted) {
      throw rejectedCommandError(
        ack,
        "The ygg host rejected this slash command.",
      );
    }
  }

  getRepositoryContext(projectId: string): Promise<RepositoryContextSnapshot> {
    return this.transport.getRepositoryContext(projectId);
  }

  getUsageStats(period: UsagePeriod): Promise<UsageStats> {
    return this.transport.getUsageStats(period);
  }

  getUsageLifetime(): Promise<LifetimeUsage> {
    return this.transport.getUsageLifetime();
  }

  getUsageActivity(): Promise<UsageActivity> {
    return this.transport.getUsageActivity();
  }

  searchTrustedFiles(
    projectId: string,
    query: string,
  ): Promise<TrustedFileSearchResult> {
    return this.transport.searchTrustedFiles(projectId, query);
  }

  private async applyGoalMutation(
    mutation: GoalMutation,
  ): Promise<GoalState | null> {
    const session = this.selectedSession;
    if (!session) throw new Error("No task is selected.");
    const goalResponse = await this.transport.updateGoal(
      session.sessionId,
      mutation,
    );
    const goal = goalResponse.goal;
    if (this.state.selectedSessionId === session.sessionId) {
      const candidateRevision = goalResponse.revision;
      if (
        acceptsGoalProjection(
          this.state.goal,
          goal,
          this.goalRevision,
          candidateRevision,
        )
      ) {
        this.goalRevision = Math.max(
          this.goalRevision,
          candidateRevision,
          goal?.revision ?? 0,
        );
        this.publish({ ...this.state, goal });
      }
    }
    return goal;
  }

  setGoal(objective: string): Promise<GoalState | null> {
    const value = objective.trim();
    if (!value)
      return Promise.reject(new Error("A goal objective is required."));
    return this.applyGoalMutation({ objective: value });
  }

  pauseGoal(): Promise<GoalState | null> {
    return this.applyGoalMutation({ action: "pause" });
  }

  resumeGoal(): Promise<GoalState | null> {
    return this.applyGoalMutation({ action: "resume" });
  }

  clearGoal(): Promise<GoalState | null> {
    return this.applyGoalMutation({ action: "clear" });
  }

  readTrustedFile(
    projectId: string,
    entryId: string,
  ): Promise<TrustedFileRead> {
    return this.transport.readTrustedFile(projectId, entryId);
  }

  getProjectFileTree(
    projectId: string,
    path?: string,
  ): Promise<ProjectFileTree> {
    return this.transport.getProjectFileTree(projectId, path);
  }

  readProjectFile(
    projectId: string,
    path: string,
    startLine?: number,
    endLine?: number,
  ): Promise<ProjectFileRead> {
    return this.transport.readProjectFile(projectId, path, startLine, endLine);
  }

  searchProjectFiles(
    projectId: string,
    query: string,
  ): Promise<ProjectFileSearchResult> {
    return this.transport.searchProjectFiles(projectId, query);
  }

  writeProjectFile(
    projectId: string,
    request: ProjectFileWriteRequest,
  ): Promise<ProjectFileWrite> {
    return this.transport.writeProjectFile(projectId, request);
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

  cancelSessionSelection(): void {
    this.selectionGeneration += 1;
    this.selectionAbort?.abort();
    this.selectionAbort = null;
    if (this.state.selectionError) {
      this.publish({ ...this.state, selectionError: null });
    }
  }

  async selectSession(
    sessionId: string,
    routeMode: SessionRouteMode = "push",
  ): Promise<void> {
    if (this.state.selectedSessionId === sessionId) {
      this.selectionGeneration += 1;
      this.selectionAbort?.abort();
      this.selectionAbort = null;
      if (this.state.selectionError) {
        this.publish({ ...this.state, selectionError: null });
      }
      if (routeMode !== "none") writeSessionRoute(sessionId, routeMode);
      return;
    }
    const generation = ++this.selectionGeneration;
    this.selectionAbort?.abort();
    const controller = new AbortController();
    this.selectionAbort = controller;
    if (this.state.selectionError) {
      this.publish({ ...this.state, selectionError: null });
    }

    try {
      const [snapshot, goalResponse] = await Promise.all([
        this.state.sessions[sessionId] ??
          this.transport.getSession(sessionId, controller.signal),
        this.transport.getGoal(sessionId, controller.signal),
      ]);
      const goal = goalResponse.goal;
      const summaryTitle = this.state.bootstrap?.sessions.find(
        (summary) => summary.id === sessionId,
      )?.title;
      const installedSnapshot = summaryTitle
        ? {
            ...snapshot,
            title: stableSessionTitle(summaryTitle, snapshot.title),
          }
        : snapshot;
      primeSessionItemIndex(installedSnapshot);
      if (
        generation !== this.selectionGeneration ||
        controller.signal.aborted
      ) {
        return;
      }
      this.goalRevision = goalResponse.revision;
      this.publish({
        ...this.state,
        bootstrap: this.state.bootstrap
          ? {
              ...this.state.bootstrap,
              sessions: this.state.bootstrap.sessions.map((summary) =>
                summary.id === sessionId
                  ? {
                      ...summary,
                      title: stableSessionTitle(
                        summary.title,
                        installedSnapshot.title,
                      ),
                      unread: false,
                    }
                  : summary,
              ),
            }
          : this.state.bootstrap,
        selectedSessionId: sessionId,
        selectionError: null,
        goal,
        sessions: {
          ...this.state.sessions,
          [sessionId]: installedSnapshot,
        },
      });
      if (routeMode !== "none") writeSessionRoute(sessionId, routeMode);
      this.selectionAbort = null;
    } catch (error) {
      if (
        (error instanceof DOMException && error.name === "AbortError") ||
        generation !== this.selectionGeneration ||
        controller.signal.aborted
      ) {
        return;
      }
      this.selectionAbort = null;
      this.publish({
        ...this.state,
        selectionError: {
          sessionId,
          routeMode,
          message:
            error instanceof Error
              ? error.message
              : "The session could not be opened.",
        },
      });
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
    const [snapshot, goalResponse] = await Promise.all([
      this.transport.getSession(sessionId),
      this.transport.getGoal(sessionId),
    ]);
    const goal = goalResponse.goal;
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
    this.goalRevision = goalResponse.revision;
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
      goal,
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
      throw rejectedCommandError(ack, "The ygg host rejected this message.");
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
    activeDelivery: "steer" | "followUp" = "steer",
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

  async configure(patch: {
    modelId?: string;
    reasoning?: ReasoningEffort;
    authority?: AuthorityProfile;
  }): Promise<void> {
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
        "A task branch can only be checked out after current work finishes.",
      );
    }
    const target = session.branches.entries.find(
      (entry) => entry.entryId === entryId && entry.checkoutable,
    );
    if (!target) {
      throw new Error("That task checkpoint is not available for checkout.");
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
      throw new Error("The ygg host did not identify the forked task.");
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
        "The ygg host rejected this task lifecycle change.",
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
    answer: { type: "text"; text: string } | { type: "choice"; choice: string },
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

  get selectedGoal(): GoalState | null {
    return this.state.goal;
  }

  dispose(): void {
    this.disposed = true;
    this.initializationGeneration += 1;
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
